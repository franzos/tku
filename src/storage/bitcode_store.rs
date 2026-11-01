use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::Storage;
use crate::atomic_write::atomic_write;
use crate::paths;
use crate::types::{Provider, UsageRecord};

/// Default ceiling on a provider's cache file. Configurable via
/// `cache_max_bytes` in `config.toml`.
///
/// The limit is set by rewrite cost, not disk: `flush` re-serializes the whole
/// provider file on every run that saw a new session, so the ceiling is also
/// the worst-case write per run. Compaction keeps a normal corpus far below it.
pub const DEFAULT_MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;

/// Fraction of the ceiling at which every run starts warning.
const WARN_FRACTION: f64 = 0.85;

/// Floor under the pre-read size guard, independent of the configured ceiling.
/// Reading is what risks an OOM; a file this size is loadable on any machine
/// that can run the scan, and refusing it would strand a store the user could
/// otherwise compact back down.
const ABSOLUTE_READ_LIMIT: u64 = 1024 * 1024 * 1024;

/// Record ages tried in turn when the ceiling is reached. Anything older than
/// the cutoff is collapsed to one record per day; the first step that brings
/// the file under the ceiling wins.
const COMPACT_CUTOFF_DAYS: &[i64] = &[180, 90, 30];

/// Marks a record produced by [`compact_file`]. Also the `request_id`, which
/// with the per-file `message_id` keeps aggregates unique under the dedup key.
const AGG_MARKER: &str = "tku-agg";

/// Prefix written ahead of every cache payload; bump on any `UsageRecord`
/// shape change. bitcode is positionally packed and carries no schema, so a
/// stale cache can decode into a new struct as plausible garbage instead of
/// failing — the marker is what makes that impossible.
///
/// The store is the only copy of usage for sessions the tool has since deleted,
/// so a bump must *migrate*, never discard: add the previous decoder to
/// [`decode_cache`] and upgrade in place. A payload no decoder understands is
/// quarantined rather than overwritten, so nothing is lost to a bad bump.
const CACHE_FORMAT_VERSION: u32 = 0x746b_7501;

/// How long `flush` waits for another process to finish writing the store.
const STORE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// One file per provider: `~/.cache/tku/{provider}.bin`
///
/// Each provider's data is loaded/flushed independently so adding
/// a new provider doesn't affect existing ones' deserialization cost.
///
/// Despite the name this is an archive as much as a cache: source transcripts
/// are deleted by the tools that write them (Claude Code drops sessions past
/// `cleanupPeriodDays`), so records here routinely outlive their source file
/// and cannot be re-derived. The ceiling triggers compaction of old records to
/// daily totals; a store still over it is written as is. Records are only
/// deleted by an explicit `--prune`.
pub struct BitcodeStorage {
    providers: HashMap<String, ProviderCache>,
    max_bytes: u64,
}

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct ProviderCache {
    files: HashMap<String, CachedFile>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    changes: Changes,
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
            max_bytes: configured_max_bytes(),
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

/// Read one provider's file, quarantining anything unreadable.
///
/// Never returns an empty cache while leaving a non-empty file in place: an
/// undecodable payload is renamed aside first, so the subsequent flush can't
/// overwrite records that were merely unreadable by this build.
fn load_provider(provider: &str, max_bytes: u64) -> ProviderCache {
    let Some(path) = paths::bitcode_cache_file(provider) else {
        return ProviderCache::default();
    };
    read_provider(&path, max_bytes, true).unwrap_or_else(|e| {
        // Readable-but-not-read (permissions, I/O error). Leave it alone
        // and run without a cache rather than replace what we can't see.
        eprintln!("tku: cannot read {provider} cache ({e}); running without it this time");
        ProviderCache::default()
    })
}

/// [`load_provider`] for a known path. Errors when the file exists but could
/// not be read, or is unusable and `quarantine_bad` is false; a missing, empty
/// or quarantined file is an empty cache.
pub(crate) fn read_provider(
    path: &Path,
    max_bytes: u64,
    quarantine_bad: bool,
) -> std::io::Result<ProviderCache> {
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
            return Ok(ProviderCache::default());
        }
    }
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProviderCache::default()),
        Err(e) => return Err(e),
    };
    if data.is_empty() {
        return Ok(ProviderCache::default());
    }
    match decode_cache(&data) {
        Some(pc) => Ok(pc),
        None if !quarantine_bad => Err(std::io::Error::other("unrecognized format")),
        None => {
            quarantine(path, "unrecognized format");
            Ok(ProviderCache::default())
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

fn decode_cache(data: &[u8]) -> Option<ProviderCache> {
    let (version, cache): (u32, ProviderCache) = bitcode::deserialize(data).ok()?;
    (version == CACHE_FORMAT_VERSION).then_some(cache)
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

/// Retag the records of account `old` as `new`, including the account segment
/// of aggregate message ids. Returns whether any record changed.
fn rekey_records(records: &mut [UsageRecord], old: &str, new: &str) -> bool {
    let agg_prefix = format!("{AGG_MARKER}:");
    let mut changed = false;
    for r in records {
        if r.account_uuid.as_deref() != Some(old) {
            continue;
        }
        if r.message_id.starts_with(&agg_prefix) {
            let suffix = format!(":{old}:{}", r.fast_mode);
            if let Some(head) = r.message_id.strip_suffix(&suffix) {
                r.message_id = format!("{head}:{new}:{}", r.fast_mode);
            }
        }
        r.account_uuid = Some(new.to_string());
        changed = true;
    }
    changed
}

/// Collapse every record in `cf` to one per `(day, model, account, fast_mode)`
/// when the whole file predates `cutoff`. Returns whether anything changed.
///
/// Reporting groups by day, so totals and per-day rows are unaffected; what is
/// lost is intra-day resolution (burn rate) and per-message detail for old
/// sessions. A file with any record at or after the cutoff is left alone, so
/// live sessions keep full detail.
fn compact_file(cf: &mut CachedFile, cutoff: DateTime<Utc>) -> bool {
    if cf.records.len() < 2 || cf.records.iter().any(|r| r.timestamp >= cutoff) {
        return false;
    }

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, UsageRecord> = HashMap::new();
    for r in cf.records.drain(..) {
        let key = format!(
            "{}:{}:{}:{}:{}",
            r.session_id,
            r.timestamp.date_naive(),
            r.model,
            r.account_uuid.as_deref().unwrap_or(""),
            r.fast_mode
        );
        match groups.get_mut(&key) {
            Some(agg) => {
                agg.input_tokens += r.input_tokens;
                agg.output_tokens += r.output_tokens;
                agg.cache_creation_input_tokens += r.cache_creation_input_tokens;
                agg.cache_creation_1h_input_tokens += r.cache_creation_1h_input_tokens;
                agg.cache_read_input_tokens += r.cache_read_input_tokens;
                // Keep the earliest timestamp so the record stays inside the
                // day it actually belongs to.
                if r.timestamp < agg.timestamp {
                    agg.timestamp = r.timestamp;
                }
            }
            None => {
                let mut agg = r;
                // Deterministic and unique per (file, day, model, account,
                // speed): re-compacting an already-compacted file is a no-op,
                // and the same session restored from two places still dedups.
                agg.message_id = format!("{AGG_MARKER}:{key}");
                agg.request_id = AGG_MARKER.to_string();
                order.push(key.clone());
                groups.insert(key, agg);
            }
        }
    }

    cf.records = order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect();
    true
}

/// Drop records that `dedup` would discard at read time anyway.
///
/// A resumed or forked session replays earlier messages into the new
/// transcript, so the same `(message_id, request_id)` legitimately appears in
/// several files. Reporting collapses them; the store keeps every copy. Files
/// are visited in sorted order so the surviving copy is the same on every run.
///
/// Returns how many records were dropped.
fn dedup_cache(pc: &mut ProviderCache) -> usize {
    let mut keys: Vec<String> = pc.files.keys().cloned().collect();
    keys.sort_unstable();

    let mut seen: HashSet<u64> = HashSet::new();
    let mut dropped = 0;
    for key in keys {
        let Some(cf) = pc.files.get_mut(&key) else {
            continue;
        };
        let before = cf.records.len();
        cf.records.retain(|r| {
            seen.insert(crate::dedup::fingerprint(
                r.provider,
                &r.message_id,
                &r.request_id,
                r.account_uuid.as_deref(),
            ))
        });
        dropped += before - cf.records.len();
    }
    dropped
}

/// Compact every file older than `cutoff`. Returns whether anything changed.
fn compact(pc: &mut ProviderCache, cutoff: DateTime<Utc>) -> bool {
    let mut changed = false;
    for cf in pc.files.values_mut() {
        changed |= compact_file(cf, cutoff);
    }
    changed
}

/// Serialize, compacting progressively older records until the payload fits.
///
/// Returns the payload plus whether the ceiling was still exceeded after every
/// compaction step — the caller writes it regardless. Dropping records to hit a
/// size target would delete history that no longer exists anywhere else, so the
/// ceiling yields rather than the archive.
pub(crate) fn serialize_within(
    pc: &mut ProviderCache,
    max_bytes: u64,
    provider: &str,
) -> Option<Vec<u8>> {
    let mut data = serialize(pc, provider)?;
    if data.len() as u64 <= max_bytes {
        return Some(data);
    }
    // Must run before any compaction: aggregates carry synthetic message ids,
    // so a duplicate that survives into one would be summed twice and never
    // collapsed again. Costs nothing observable — read-time dedup discards
    // these records anyway.
    if dedup_cache(pc) > 0 {
        data = serialize(pc, provider)?;
        if data.len() as u64 <= max_bytes {
            return Some(data);
        }
    }
    for &days in COMPACT_CUTOFF_DAYS {
        let cutoff = Utc::now() - Duration::days(days);
        if !compact(pc, cutoff) {
            continue;
        }
        data = serialize(pc, provider)?;
        eprintln!(
            "tku: {provider} cache over the {} ceiling; compacted records older than {days} days to {}",
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
        "tku: {provider} cache is {} and compaction could not get it under the {} ceiling. \
         Writing it as is; no records were dropped. Raise cache_max_bytes in {config}.",
        human(data.len() as u64),
        human(max_bytes)
    );
    Some(data)
}

fn serialize(pc: &ProviderCache, provider: &str) -> Option<Vec<u8>> {
    match bitcode::serialize(&(CACHE_FORMAT_VERSION, pc)) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("tku: failed to serialize {provider} cache: {e}");
            None
        }
    }
}

pub(crate) fn human(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MiB", bytes as f64 / MIB)
}

/// Take the store lock beside `path`, waiting up to [`STORE_LOCK_WAIT`].
/// `None` (after a warning) when it could not be taken.
pub(crate) fn lock_store(path: &Path, provider: &str) -> Option<StoreLock> {
    let dir = path.parent()?;
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("tku: failed to create {}: {e}", dir.display());
        return None;
    }
    let lock_path = dir.join(".store.lock");
    match StoreLock::acquire_wait(&lock_path, STORE_LOCK_WAIT) {
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
                "tku: cannot lock {} ({e}); {provider} cache not saved this run",
                lock_path.display()
            );
            None
        }
    }
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
        let pc = self.provider_cache(provider);
        let key = file_path.to_string_lossy().to_string();
        pc.changes.touch(&key);
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
        }
    }

    fn flush(&mut self) -> bool {
        if !self.providers.values().any(|pc| pc.dirty) {
            return true;
        }
        let max_bytes = self.max_bytes;
        let mut ok = true;
        for (name, pc) in &mut self.providers {
            if !pc.dirty {
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
            merge_from_disk(pc, disk, &changes);
            let Some(data) = serialize_within(pc, max_bytes, name) else {
                pc.changes = changes;
                ok = false;
                continue;
            };
            if let Err(e) = atomic_write(&path, &data, None) {
                eprintln!("tku: failed to write {name} cache: {e}");
                pc.changes = changes;
                ok = false;
                continue;
            }
            pc.dirty = false;
            // Only the approaching-the-ceiling case: once it is actually
            // exceeded, `serialize_within` has already said what it did.
            let used = data.len() as u64;
            if used <= max_bytes && used as f64 >= max_bytes as f64 * WARN_FRACTION {
                eprintln!(
                    "tku: {name} cache at {:.0}% of the {} ceiling ({}). \
                     Past the ceiling, records older than 180 days are compacted to daily totals \
                     (totals are kept, intra-day detail is not); raise cache_max_bytes to keep full detail.",
                    used as f64 / max_bytes as f64 * 100.0,
                    human(max_bytes),
                    human(used)
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
        let mut all = Vec::new();
        for (_, mut pc) in self.providers.drain() {
            for (_, cf) in pc.files.drain() {
                all.extend(cf.records);
            }
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
        }
    }

    fn file(records: Vec<UsageRecord>) -> CachedFile {
        CachedFile {
            mtime_secs: 0,
            size: 0,
            records,
        }
    }

    #[test]
    fn compaction_preserves_daily_totals() {
        let mut cf = file(vec![
            rec("2020-01-01T01:00:00Z", "opus", 10),
            rec("2020-01-01T02:00:00Z", "opus", 5),
            rec("2020-01-02T01:00:00Z", "opus", 7),
        ]);
        assert!(compact_file(&mut cf, Utc::now()));

        assert_eq!(cf.records.len(), 2);
        let day1 = &cf.records[0];
        assert_eq!(day1.output_tokens, 15);
        assert_eq!(day1.input_tokens, 2);
        assert_eq!(day1.cache_read_input_tokens, 6);
        // Earliest timestamp of the group, so the record stays in its own day.
        assert_eq!(day1.timestamp.to_rfc3339(), "2020-01-01T01:00:00+00:00");
        assert_eq!(cf.records[1].output_tokens, 7);
    }

    #[test]
    fn compaction_splits_by_model() {
        let mut cf = file(vec![
            rec("2020-01-01T01:00:00Z", "opus", 10),
            rec("2020-01-01T02:00:00Z", "sonnet", 5),
        ]);
        assert!(compact_file(&mut cf, Utc::now()));
        assert_eq!(cf.records.len(), 2);
    }

    /// The whole point of the ceiling path: it must be safe to run twice.
    #[test]
    fn compaction_is_idempotent() {
        let mut cf = file(vec![
            rec("2020-01-01T01:00:00Z", "opus", 10),
            rec("2020-01-01T02:00:00Z", "opus", 5),
        ]);
        compact_file(&mut cf, Utc::now());
        let first: Vec<_> = cf.records.iter().map(|r| r.message_id.clone()).collect();
        let total = cf.records[0].output_tokens;

        compact_file(&mut cf, Utc::now());
        let second: Vec<_> = cf.records.iter().map(|r| r.message_id.clone()).collect();
        assert_eq!(first, second);
        assert_eq!(cf.records[0].output_tokens, total);
    }

    /// Aggregates must survive `dedup`, which keys on message_id + request_id.
    #[test]
    fn compacted_records_survive_dedup() {
        let mut cf = file(vec![
            rec("2020-01-01T01:00:00Z", "opus", 10),
            rec("2020-01-02T01:00:00Z", "opus", 5),
        ]);
        compact_file(&mut cf, Utc::now());
        let deduped = crate::dedup::dedup(cf.records.clone());
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn a_file_with_live_records_is_left_alone() {
        let mut cf = file(vec![
            rec("2020-01-01T01:00:00Z", "opus", 10),
            rec("2020-01-01T02:00:00Z", "opus", 5),
        ]);
        let cutoff = "2019-01-01T00:00:00Z".parse().unwrap();
        assert!(!compact_file(&mut cf, cutoff));
        assert_eq!(cf.records.len(), 2);
    }

    /// A resumed session replays earlier messages into a new transcript, so the
    /// same record sits in two files and reporting collapses them. Compaction
    /// replaces message ids, so the duplicate must be gone *before* it runs or
    /// its tokens get counted twice and can never be collapsed again.
    #[test]
    fn duplicates_are_dropped_before_compaction_can_bake_them_in() {
        let shared = rec("2020-01-01T01:00:00Z", "opus", 10);
        let mut pc = ProviderCache::default();
        pc.files
            .insert("/a.jsonl".into(), file(vec![shared.clone()]));
        pc.files.insert(
            "/b.jsonl".into(),
            file(vec![shared.clone(), rec("2020-01-01T03:00:00Z", "opus", 4)]),
        );

        assert_eq!(dedup_cache(&mut pc), 1);
        compact(&mut pc, Utc::now());

        let total: u64 = pc
            .files
            .values()
            .flat_map(|cf| &cf.records)
            .map(|r| r.output_tokens)
            .sum();
        assert_eq!(total, 14, "the shared record must be counted once");
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

        let data = serialize_within(&mut pc, 64, "claude").unwrap();
        assert!(
            data.len() > 64,
            "compacted payload must still exceed the ceiling"
        );

        let decoded = decode_cache(&data).unwrap();
        assert_eq!(decoded.files.keys().cloned().collect::<HashSet<_>>(), keys);
        assert_eq!(sum(&decoded), before);
    }

    #[test]
    fn unrecognized_payload_decodes_to_nothing_rather_than_garbage() {
        assert!(decode_cache(b"not a cache").is_none());
        let wrong = bitcode::serialize(&(0u32, ProviderCache::default())).unwrap();
        assert!(decode_cache(&wrong).is_none());
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
            let cf = storage.provider_cache("claude").files.get_mut(key).unwrap();
            assert!(compact_file(cf, Utc::now()));
            storage.provider_cache("claude").dirty = false;

            storage.rekey_account("old", "new", None, None);

            assert!(storage.provider_cache("claude").dirty);
            let rekeyed = records_of(&mut storage, key);
            for r in &rekeyed {
                assert_eq!(r.account_uuid.as_deref(), Some("new"));
                assert!(r.message_id.ends_with(":new:false"), "{}", r.message_id);
            }

            let cf = storage.provider_cache("claude").files.get_mut(key).unwrap();
            compact_file(cf, Utc::now());
            let recompacted = records_of(&mut storage, key);
            let ids =
                |rs: &[UsageRecord]| rs.iter().map(|r| r.message_id.clone()).collect::<Vec<_>>();
            assert_eq!(ids(&rekeyed), ids(&recompacted));
            assert_eq!(recompacted[0].output_tokens, 15);
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
            let cf = storage
                .provider_cache("claude")
                .files
                .get_mut("/proj/other.jsonl")
                .unwrap();
            compact_file(cf, Utc::now());
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
            a.provider_cache("claude").dirty = true;
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
            assert!(x[0].message_id.starts_with(AGG_MARKER));
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
}
