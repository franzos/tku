use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::Storage;
use crate::atomic_write::atomic_write;
use crate::paths;
use crate::types::UsageRecord;

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

/// One file per provider: `~/.cache/tku/{provider}.bin`
///
/// Each provider's data is loaded/flushed independently so adding
/// a new provider doesn't affect existing ones' deserialization cost.
///
/// Despite the name this is an archive as much as a cache: source transcripts
/// are deleted by the tools that write them (Claude Code drops sessions past
/// `cleanupPeriodDays`), so records here routinely outlive their source file
/// and cannot be re-derived. The ceiling is honoured by compacting old records
/// to daily totals rather than dropping them; records are only ever deleted by
/// an explicit `--prune`, or as a last resort once compaction cannot free
/// enough — and then the ceiling yields before anything still on disk does.
pub struct BitcodeStorage {
    providers: HashMap<String, ProviderCache>,
    max_bytes: u64,
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
            max_bytes: crate::config::load_config()
                .cache_max_bytes
                .unwrap_or(DEFAULT_MAX_CACHE_BYTES),
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

/// Read one provider's file, quarantining anything unreadable.
///
/// Never returns an empty cache while leaving a non-empty file in place: an
/// undecodable payload is renamed aside first, so the subsequent flush can't
/// overwrite records that were merely unreadable by this build.
fn load_provider(provider: &str, max_bytes: u64) -> ProviderCache {
    let Some(path) = paths::bitcode_cache_file(provider) else {
        return ProviderCache::default();
    };
    // Pre-flight size check. `fs::read` allocates a Vec sized to the file, so
    // a runaway file would OOM the process before we could compact it. The
    // bound is absolute, not a multiple of the ceiling: lowering `cache_max_bytes`
    // must compact an existing store down, not refuse to open it.
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > max_bytes.saturating_mul(2).max(ABSOLUTE_READ_LIMIT) {
            quarantine(
                &path,
                &format!("{} bytes, over twice the ceiling", meta.len()),
            );
            return ProviderCache::default();
        }
    }
    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ProviderCache::default(),
        Err(e) => {
            // Readable-but-not-read (permissions, I/O error). Leave it alone
            // and run without a cache rather than replace what we can't see.
            eprintln!("tku: cannot read {provider} cache ({e}); running without it this time");
            return ProviderCache::default();
        }
    };
    if data.is_empty() {
        return ProviderCache::default();
    }
    match decode_cache(&data) {
        Some(pc) => pc,
        None => {
            quarantine(&path, "unrecognized format");
            ProviderCache::default()
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
fn serialize_within(pc: &mut ProviderCache, max_bytes: u64, provider: &str) -> Option<Vec<u8>> {
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
    // Last resort, and the only eviction that actually shrinks anything:
    // see `evict_oldest_orphans`.
    let dropped = evict_oldest_orphans(pc, max_bytes, provider)?;
    data = serialize(pc, provider)?;
    let config = paths::config_file()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "config.toml".into());
    if data.len() as u64 <= max_bytes {
        eprintln!(
            "tku: {provider} cache still over the {} ceiling after compaction; \
             dropped the {dropped} oldest sessions whose transcripts are already gone ({}). \
             That history is not recoverable — raise cache_max_bytes in {config} to stop this.",
            human(max_bytes),
            human(data.len() as u64)
        );
    } else {
        eprintln!(
            "tku: {provider} cache is {} and cannot be brought under the {} ceiling: \
             the transcripts still on disk alone exceed it. Writing it anyway rather than \
             deleting records that are still readable. Raise cache_max_bytes in {config}.",
            human(data.len() as u64),
            human(max_bytes)
        );
    }
    Some(data)
}

/// Drop whole files, oldest source mtime first, until the payload fits —
/// considering only entries whose source transcript no longer exists.
///
/// Evicting an entry whose file is still on disk frees nothing: the next run
/// re-parses and re-inserts it, so the cache oscillates at the same size while
/// paying the parse each time. Orphans are the only entries that stay gone,
/// which also makes them the only ones whose loss is permanent — hence last
/// resort, after every compaction step, and loudly.
fn evict_oldest_orphans(pc: &mut ProviderCache, max_bytes: u64, provider: &str) -> Option<usize> {
    let mut by_age: Vec<(i64, String)> = pc
        .files
        .iter()
        .filter(|(k, _)| !Path::new(k.as_str()).exists())
        .map(|(k, cf)| (cf.mtime_secs, k.clone()))
        .collect();
    if by_age.is_empty() {
        return Some(0);
    }
    by_age.sort_unstable();

    // Re-serialize in batches rather than per file: the goal is to get back
    // under a ceiling already blown, not to find the minimal cut.
    let batch = (by_age.len() / 20).max(1);
    let mut dropped = 0;
    for chunk in by_age.chunks(batch) {
        for (_, key) in chunk {
            pc.files.remove(key);
            dropped += 1;
        }
        if serialize(pc, provider)?.len() as u64 <= max_bytes {
            break;
        }
    }
    Some(dropped)
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

fn human(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MiB", bytes as f64 / MIB)
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
        let dropped: usize = pc
            .files
            .iter()
            .filter(|(k, _)| !known.contains(*k))
            .map(|(_, cf)| cf.records.len())
            .sum();
        pc.files.retain(|k, _| known.contains(k));
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

    fn flush(&mut self) {
        let Some(dir) = paths::cache_dir() else {
            return;
        };
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("tku: failed to create cache dir: {e}");
            return;
        }

        let max_bytes = self.max_bytes;
        for (name, pc) in &mut self.providers {
            if !pc.dirty {
                continue;
            }
            let Some(data) = serialize_within(pc, max_bytes, name) else {
                continue;
            };
            let Some(path) = paths::bitcode_cache_file(name) else {
                continue;
            };
            if let Err(e) = atomic_write(&path, &data, None) {
                eprintln!("tku: failed to write {name} cache: {e}");
                continue;
            }
            // Only the approaching-the-ceiling case: once it is actually
            // exceeded, `serialize_within` has already said what it did.
            let used = data.len() as u64;
            if used <= max_bytes && used as f64 >= max_bytes as f64 * WARN_FRACTION {
                eprintln!(
                    "tku: {name} cache at {:.0}% of the {} ceiling ({}). \
                     Records older than 180 days will be compacted to daily totals when it fills; \
                     raise cache_max_bytes to keep full detail.",
                    used as f64 / max_bytes as f64 * 100.0,
                    human(max_bytes),
                    human(used)
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Provider;

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
}
