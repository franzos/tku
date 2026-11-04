//! Upgrades legacy bitcode stores to the native format.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::bitcode_store::{self, BitcodeStorage, ProviderCache};
use super::relocate::with_suffix;
use super::{format, Storage};
use crate::atomic_write::{atomic_write, sync_dir};
use crate::dedup::Tokens;
use crate::paths;
use crate::progress::Progress;
use crate::types::{Provider, UsageRecord};

/// Runs after `RELOCATE`, so every store is in the data dir.
pub const STORE_FORMAT: crate::migrate::Migration = crate::migrate::Migration {
    label: "store format",
    pending,
    run,
};

/// Infix of a store moved aside as unusable, e.g. by an older tku meeting the native format.
const QUARANTINE: &str = ".bin.quarantine-";
const UNREADABLE: &str = ".bin.unreadable-";

#[derive(Default)]
struct Work {
    /// `<provider>.bin` is non-empty without the native magic.
    legacy: bool,
    /// Quarantined stores this build can decode, sorted.
    quarantines: Vec<PathBuf>,
}

fn pending() -> bool {
    paths::data_dir().is_some_and(|dir| !work(&dir).is_empty()) || sqlite_pending()
}

#[cfg(feature = "sqlite")]
fn sqlite_pending() -> bool {
    paths::sqlite_db_file().is_some_and(|db| super::sqlite_store::has_legacy_files(&db))
}

#[cfg(not(feature = "sqlite"))]
fn sqlite_pending() -> bool {
    false
}

/// Providers with a store to upgrade, from file headers only.
fn work(dir: &Path) -> Vec<(Provider, Work)> {
    let mut found: Vec<(Provider, Work)> = Provider::iter().map(|p| (p, Work::default())).collect();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some((provider, quarantined)) = name.to_str().and_then(classify) else {
                continue;
            };
            let Ok(head) = header(&entry.path()) else {
                continue;
            };
            let Some((_, w)) = found.iter_mut().find(|(p, _)| *p == provider) else {
                continue;
            };
            if quarantined {
                if format::is_current(&head) {
                    w.quarantines.push(entry.path());
                }
            } else if !head.is_empty() && !format::is_native(&head) {
                w.legacy = true;
            }
        }
    }
    found.retain(|(_, w)| w.legacy || !w.quarantines.is_empty());
    for (_, w) in &mut found {
        w.quarantines.sort();
    }
    found
}

/// The provider a data-dir entry belongs to, and whether it is a quarantined store.
fn classify(name: &str) -> Option<(Provider, bool)> {
    let (base, quarantined) = match name.split_once(QUARANTINE) {
        Some((base, _)) => (base, true),
        None => (name.strip_suffix(".bin")?, false),
    };
    Provider::iter()
        .find(|p| p.as_str() == base)
        .map(|p| (p, quarantined))
}

fn header(path: &Path) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(format::HEADER_LEN);
    fs::File::open(path)?
        .take(format::HEADER_LEN as u64)
        .read_to_end(&mut head)?;
    Ok(head)
}

fn run(progress: &Progress) -> Result<String> {
    let mut lines = upgrade_stores(progress, bitcode_store::STORE_LOCK_WAIT)?;
    if !lines.is_empty() {
        rescan(&mut BitcodeStorage::new(), progress, &mut lines);
    }
    #[cfg(feature = "sqlite")]
    if let Some((line, mut store)) = upgrade_sqlite(progress)? {
        lines.push(line);
        rescan(&mut store, progress, &mut lines);
    }
    if lines.is_empty() {
        return Ok("nothing to upgrade".to_string());
    }
    Ok(lines.join("; "))
}

/// Scan every provider into `store` and write it all.
fn rescan(store: &mut dyn Storage, progress: &Progress, lines: &mut Vec<String>) {
    crate::providers::scan_all(store, progress, false);
    store.force_write();
    progress.show("Writing the record store...");
    if !store.flush() {
        lines.push("the rescan was not saved and runs again on the next start".to_string());
    }
}

/// Upgrade the sqlite database's legacy files in place, keeping a copy of the
/// database as `<db>.legacy` the first time. Returns the result line and the
/// opened store for the rescan.
#[cfg(feature = "sqlite")]
fn upgrade_sqlite(
    progress: &Progress,
) -> Result<Option<(String, super::sqlite_store::SqliteStorage)>> {
    use super::sqlite_store;

    let Some(db) = paths::sqlite_db_file() else {
        return Ok(None);
    };
    if !sqlite_store::has_legacy_files(&db) {
        return Ok(None);
    }
    progress.show("Upgrading the sqlite store...");
    let backup = with_suffix(&db, ".legacy");
    let kept = if backup.exists() {
        None
    } else {
        sqlite_store::backup(&db, &backup).context("keep the legacy database")?;
        Some(backup)
    };
    let mut store = sqlite_store::SqliteStorage::open()?;
    let done = store.upgrade_legacy_files(progress)?;
    let mut line = format!(
        "sqlite: refreshed {} files from source, kept {} merged, left {} to the rescan",
        done.refreshed,
        done.upgraded.saturating_sub(done.refreshed + done.rescan),
        done.rescan
    );
    if done.unreadable > 0 {
        line.push_str(&format!(
            ", skipped {} with unreadable rows",
            done.unreadable
        ));
    }
    if let Some(kept) = kept {
        let size = fs::metadata(&kept).map_or(0, |m| m.len());
        line.push_str(&format!(
            ", legacy database kept at {} ({})",
            kept.display(),
            bitcode_store::human(size)
        ));
    }
    Ok(Some((line, store)))
}

/// Upgrade every provider store that needs it, one result line each.
fn upgrade_stores(progress: &Progress, wait: Duration) -> Result<Vec<String>> {
    let dir = paths::data_dir().ok_or_else(|| anyhow!("no data dir"))?;
    let mut lines = Vec::new();
    for (provider, _) in work(&dir) {
        if let Some(line) = upgrade_provider(&dir, provider, progress, wait)? {
            lines.push(line);
        }
    }
    Ok(lines)
}

fn upgrade_provider(
    dir: &Path,
    provider: Provider,
    progress: &Progress,
    wait: Duration,
) -> Result<Option<String>> {
    let name = provider.as_str();
    let path = dir.join(format!("{name}.bin"));
    let _lock = bitcode_store::try_lock_store(&path, wait)
        .with_context(|| format!("lock the {name} store"))?
        .ok_or_else(|| {
            anyhow!(
                "another tku process held the {name} store lock for {}s",
                wait.as_secs()
            )
        })?;
    // Re-read under the lock: another process may have upgraded it meanwhile.
    let Some((_, work)) = work(dir).into_iter().find(|(p, _)| *p == provider) else {
        return Ok(None);
    };
    progress.show(format_args!("Upgrading the {name} store..."));

    let main = bitcode_store::read_provider(&path, bitcode_store::configured_max_bytes(), true)
        .with_context(|| format!("read {}", path.display()))?;
    let legacy = main.as_ref().is_some_and(|pc| pc.legacy);
    let mut pc = main.unwrap_or_default();
    pc.legacy = false;

    let mut folded = Vec::new();
    for q in &work.quarantines {
        let data = fs::read(q).with_context(|| format!("read {}", q.display()))?;
        match format::decode(&data) {
            Some(qpc) => {
                fold(&mut pc, qpc);
                folded.push(q);
            }
            None => set_aside(q)?,
        }
    }
    if !legacy && folded.is_empty() {
        return Ok(work
            .legacy
            .then(|| format!("{name}: unusable store quarantined")));
    }
    if !legacy {
        dedup_files(&mut pc);
    }

    let mut parts = Vec::new();
    let mut kept = None;
    if legacy {
        // Only the first upgrade's bytes are the original archive; a legacy file
        // beside quarantines was rewritten by an older binary.
        if folded.is_empty() && !has_backup(dir, name)? {
            let to = with_suffix(&path, ".legacy");
            preserve(&path, &to).context("keep the legacy store")?;
            kept = Some(to);
        }
        let total = pc.files.len();
        let (refreshed, rescan) = upgrade_cache(name, &mut pc, progress);
        parts.push(format!(
            "refreshed {refreshed} files from source, kept {} merged, left {rescan} to the rescan",
            total - refreshed - rescan
        ));
    }

    progress.show(format_args!("Writing the {name} store..."));
    atomic_write(&path, &format::encode(&pc), None)
        .with_context(|| format!("write {}", path.display()))?;
    sync_dir(dir)?;
    for q in &folded {
        fs::remove_file(q).with_context(|| format!("remove {}", q.display()))?;
    }
    if !folded.is_empty() {
        sync_dir(dir)?;
        parts.push(format!(
            "folded {} quarantined store(s) back in",
            folded.len()
        ));
    }
    if let Some(kept) = kept {
        let size = fs::metadata(&kept).map_or(0, |m| m.len());
        parts.push(format!(
            "legacy store kept at {} ({})",
            kept.display(),
            bitcode_store::human(size)
        ));
    }
    Ok(Some(format!("{name}: {}", parts.join(", "))))
}

/// Union of keys; the newer `mtime_secs` wins, `from` on a tie.
fn fold(into: &mut ProviderCache, from: ProviderCache) {
    for (key, cf) in from.files {
        if into
            .files
            .get(&key)
            .is_some_and(|m| m.mtime_secs > cf.mtime_secs)
        {
            continue;
        }
        into.files.insert(key, cf);
    }
}

/// Rename a native-headed quarantine that does not decode, so it stops making the upgrade pending.
fn set_aside(q: &Path) -> Result<()> {
    let name = q
        .file_name()
        .map(|n| n.to_string_lossy().replacen(QUARANTINE, UNREADABLE, 1))
        .ok_or_else(|| anyhow!("{} has no file name", q.display()))?;
    let to = q.with_file_name(name);
    fs::rename(q, &to).with_context(|| format!("rename {}", q.display()))?;
    eprintln!(
        "tku: {} does not decode; kept at {}",
        q.display(),
        to.display()
    );
    Ok(())
}

/// Whether a `<provider>.bin.legacy*` file exists in `dir`.
fn has_backup(dir: &Path, provider: &str) -> io::Result<bool> {
    let prefix = format!("{provider}.bin.legacy");
    for entry in fs::read_dir(dir)? {
        if entry?.file_name().to_string_lossy().starts_with(&prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Keep `path`'s bytes at the free name `kept`: a hard link where possible, a
/// synced copy otherwise. `path` itself stays in place.
pub(super) fn preserve(path: &Path, kept: &Path) -> io::Result<()> {
    if fs::hard_link(path, kept).is_err() {
        copy_synced(path, kept)?;
    }
    sync_dir(kept.parent().unwrap_or(Path::new(".")))
}

fn copy_synced(src: &Path, dst: &Path) -> io::Result<()> {
    let data = fs::read(src)?;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    f.write_all(&data)
        .and_then(|()| f.sync_all())
        .inspect_err(|_| {
            let _ = fs::remove_file(dst);
        })
}

/// Upgrade a cache decoded from the legacy format in memory: merge each file's
/// copies, then [`refresh`] it. Returns how many files were re-parsed and how many sources changed.
pub(super) fn upgrade_cache(
    provider: &str,
    pc: &mut ProviderCache,
    progress: &Progress,
) -> (usize, usize) {
    pc.legacy = false;
    dedup_files(pc);
    refresh(provider, pc, progress)
}

fn dedup_files(pc: &mut ProviderCache) {
    for cf in pc.files.values_mut() {
        cf.records = crate::dedup::dedup(std::mem::take(&mut cf.records));
    }
}

/// Re-parse every file whose source is unchanged and raise its stored tokens
/// to the most complete copy. Returns how many were re-parsed and how many sources changed.
///
/// Only fingerprints already in the stored entry are raised: a file the
/// ceiling compaction trimmed keeps messages another file or its own
/// aggregate already counts out of it.
fn refresh(provider: &str, pc: &mut ProviderCache, progress: &Progress) -> (usize, usize) {
    let mut want = HashMap::new();
    let mut rescan = 0;
    for (key, cf) in &pc.files {
        let Some(df) = crate::providers::discovered_file(Path::new(key)) else {
            continue;
        };
        if (df.mtime, df.size) == (cf.mtime_secs, cf.size) {
            want.insert(key.clone(), (df.mtime, df.size));
        } else {
            rescan += 1;
        }
    }
    let Some(source) = crate::providers::all_providers()
        .into_iter()
        .find(|p| p.id().as_str() == provider)
    else {
        return (0, rescan);
    };
    if want.is_empty() {
        return (0, rescan);
    }
    let mut capture = Reparse {
        want,
        parsed: HashMap::new(),
    };
    let label = format!("Refreshing {provider} sessions");
    let cb = |current: usize, total: usize| progress.count(&label, current, total);
    source.discover_and_parse(
        &mut capture,
        progress.is_active().then_some(&cb as &dyn Fn(usize, usize)),
        false,
    );
    let refreshed = capture.parsed.len();
    for (key, fresh) in capture.parsed {
        if let Some(cf) = pc.files.get_mut(&key) {
            raise(&mut cf.records, fresh);
        }
    }
    (refreshed, rescan)
}

/// Give each stored copy the tokens of the most complete fresh copy of the same
/// message, when that copy beats it (see [`crate::dedup::merge_copy`]).
fn raise(stored: &mut [UsageRecord], fresh: Vec<UsageRecord>) {
    let fp = |r: &UsageRecord| {
        crate::dedup::stable_fingerprint(r.provider, &r.message_id, &r.request_id)
    };
    let mut best: HashMap<u64, Tokens> = HashMap::new();
    for r in fresh.iter().filter(|r| super::rollup::keyed(r)) {
        let tokens = Tokens::of(r);
        best.entry(fp(r))
            .and_modify(|b| {
                if tokens.beats(*b) {
                    *b = tokens;
                }
            })
            .or_insert(tokens);
    }
    for r in stored.iter_mut().filter(|r| super::rollup::keyed(r)) {
        if let Some(&b) = best.get(&fp(r)) {
            if b.beats(Tokens::of(r)) {
                b.apply(r);
            }
        }
    }
}

/// Collects a provider's parse of the files in `want`, reporting every other file as cached.
struct Reparse {
    want: HashMap<String, (i64, u64)>,
    parsed: HashMap<String, Vec<UsageRecord>>,
}

impl Storage for Reparse {
    fn is_cached(&mut self, _provider: &str, file_path: &Path, mtime: i64, size: u64) -> bool {
        self.want.get(file_path.to_string_lossy().as_ref()) != Some(&(mtime, size))
    }

    fn insert(
        &mut self,
        _provider: &str,
        file_path: &Path,
        _mtime: i64,
        _size: u64,
        records: Vec<UsageRecord>,
    ) {
        self.parsed
            .insert(file_path.to_string_lossy().into_owned(), records);
    }

    fn prune(&mut self, _provider: &str, _existing: &[PathBuf]) {}

    fn flush(&mut self) -> bool {
        true
    }

    fn drain_all(&mut self) -> Vec<UsageRecord> {
        Vec::new()
    }

    fn rekey_account(
        &mut self,
        _old: &str,
        _new: &str,
        _old_dir: Option<&Path>,
        _new_dir: Option<&Path>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::bitcode_store::CachedFile;
    use crate::storage::{legacy, with_scratch_home};

    const FIXTURE_DIR: &str = "/home/user/.claude/projects/-home-user-git-app";
    const ACCT: &str = "3f2a9c1e-7b4d-4e8a-9c21-5d6e7f8a9b0c";

    fn store() -> PathBuf {
        paths::bitcode_cache_file("claude").unwrap()
    }

    fn quiet() -> Progress {
        Progress::new(false)
    }

    fn rec(msg: &str, out: u64) -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: "s".into(),
            timestamp: "2026-10-01T09:00:00Z".parse().unwrap(),
            project: "p".into(),
            model: "m".into(),
            message_id: msg.into(),
            request_id: format!("r{msg}"),
            input_tokens: 1,
            output_tokens: out,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: false,
            account_uuid: None,
            aggregate: false,
        }
    }

    fn cache(files: &[(&str, i64, Vec<UsageRecord>)], legacy: bool) -> ProviderCache {
        let mut pc = ProviderCache {
            legacy,
            ..ProviderCache::default()
        };
        for (key, mtime_secs, records) in files {
            pc.files.insert(
                (*key).to_string(),
                CachedFile {
                    mtime_secs: *mtime_secs,
                    size: 1,
                    records: records.clone(),
                    sealed: Vec::new(),
                },
            );
        }
        pc
    }

    fn write_legacy(files: &[(&str, i64, Vec<UsageRecord>)]) -> Vec<u8> {
        let data = legacy::encode(&cache(files, true)).unwrap();
        fs::create_dir_all(store().parent().unwrap()).unwrap();
        fs::write(store(), &data).unwrap();
        data
    }

    fn read_store() -> ProviderCache {
        bitcode_store::read_provider(&store(), u64::MAX, false)
            .unwrap()
            .unwrap()
    }

    fn is_native_on_disk() -> bool {
        format::is_native(&fs::read(store()).unwrap())
    }

    fn outputs(pc: &ProviderCache, key: &str) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = pc.files[key]
            .records
            .iter()
            .map(|r| (r.message_id.clone(), r.output_tokens))
            .collect();
        out.sort();
        out
    }

    fn line(msg: &str, req: &str, ts: &str, input: u64, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"{req}","message":{{"id":"{msg}","model":"claude-opus-4-1-20250805","usage":{{"input_tokens":{input},"output_tokens":{out},"cache_creation_input_tokens":1200,"cache_read_input_tokens":18000}}}}}}"#
        )
    }

    #[test]
    fn upgrades_the_fixture_end_to_end() {
        with_scratch_home("upgrade-fixture", || {
            let session = |n: u8| format!("a1b2c3d4-000{n}-4000-8000-00000000000{n}");
            let root = paths::transcripts_dir("claude")
                .unwrap()
                .join(ACCT)
                .join("projects")
                .join("-home-user-git-app");
            fs::create_dir_all(&root).unwrap();
            let source = |n: u8| root.join(format!("{}.jsonl", session(n)));
            let key = |n: u8| source(n).to_string_lossy().into_owned();

            let mut pc =
                legacy::decode(include_bytes!("../../tests/fixtures/legacy-claude.bin")).unwrap();
            pc.files = pc
                .files
                .drain()
                .map(|(k, cf)| (k.replace(FIXTURE_DIR, &root.to_string_lossy()), cf))
                .collect();
            // A first-wins trim kept only the partial line of the streamed message.
            pc.files.get_mut(&key(1)).unwrap().records.remove(2);

            fs::write(
                source(1),
                [
                    line(
                        "msg_01StreamAAAAAAAAAAAAAAA",
                        "req_011StreamAAAAAAAAAAAAA",
                        "2026-10-01T09:00:00.123Z",
                        3,
                        6,
                    ),
                    line(
                        "msg_01StreamAAAAAAAAAAAAAAA",
                        "req_011StreamAAAAAAAAAAAAA",
                        "2026-10-01T09:00:01.456Z",
                        3,
                        6,
                    ),
                    line(
                        "msg_01StreamAAAAAAAAAAAAAAA",
                        "req_011StreamAAAAAAAAAAAAA",
                        "2026-10-01T09:00:02.789Z",
                        3,
                        410,
                    ),
                    line(
                        "msg_01SharedBBBBBBBBBBBBBBB",
                        "req_011SharedBBBBBBBBBBBBB",
                        "2026-10-01T09:05:00Z",
                        5,
                        120,
                    ),
                    line(
                        "msg_01FastCCCCCCCCCCCCCCCCC",
                        "req_011FastCCCCCCCCCCCCCCC",
                        "2026-10-01T09:10:00Z",
                        7,
                        90,
                    ),
                    line(
                        "msg_01ExtraXXXXXXXXXXXXXXXX",
                        "req_011ExtraXXXXXXXXXXXXXX",
                        "2026-10-01T09:20:00Z",
                        1,
                        999,
                    ),
                ]
                .join("\n")
                    + "\n",
            )
            .unwrap();
            fs::write(
                source(4),
                line(
                    "msg_01CeilFFFFFFFFFFFFFFFF2",
                    "req_011CeilFFFFFFFFFFFFFFF2",
                    "2026-10-03T10:00:00Z",
                    9,
                    900,
                ) + "\n",
            )
            .unwrap();
            // The ceiling-trimmed file: its source still holds every line its aggregates summed or dropped.
            fs::write(
                source(5),
                [
                    line(
                        "msg_01CeilFFFFFFFFFFFFFFFF1",
                        "req_011CeilFFFFFFFFFFFFFFF1",
                        "2025-01-20T10:00:00Z",
                        8,
                        80,
                    ),
                    line(
                        "msg_01CeilGGGGGGGGGGGGGGGGG",
                        "req_011CeilGGGGGGGGGGGGGGG",
                        "2025-01-21T11:00:00Z",
                        2,
                        6,
                    ),
                    line(
                        "msg_01CeilGGGGGGGGGGGGGGGGG",
                        "req_011CeilGGGGGGGGGGGGGGG",
                        "2025-01-21T11:00:01Z",
                        2,
                        6,
                    ),
                    line(
                        "msg_01CeilGGGGGGGGGGGGGGGGG",
                        "req_011CeilGGGGGGGGGGGGGGG",
                        "2025-01-21T11:00:02Z",
                        2,
                        410,
                    ),
                    line(
                        "msg_01CeilHHHHHHHHHHHHHHHHH",
                        "req_011CeilHHHHHHHHHHHHHHH",
                        "2025-01-21T12:00:00Z",
                        6,
                        60,
                    ),
                ]
                .join("\n")
                    + "\n",
            )
            .unwrap();
            for n in [1, 5] {
                let df = crate::providers::discovered_file(&source(n)).unwrap();
                let cf = pc.files.get_mut(&key(n)).unwrap();
                (cf.mtime_secs, cf.size) = (df.mtime, df.size);
            }
            let before = pc
                .snapshot()
                .iter()
                .map(|f| (f.0.to_string(), f.4.to_vec()))
                .collect::<HashMap<_, _>>();
            let original = legacy::encode(&pc).unwrap();
            fs::create_dir_all(store().parent().unwrap()).unwrap();
            fs::write(store(), &original).unwrap();

            assert!(pending());
            let lines = upgrade_stores(&quiet(), Duration::ZERO).unwrap();
            assert!(!pending());
            assert!(is_native_on_disk());
            let kept = with_suffix(&store(), ".legacy");
            assert_eq!(fs::read(&kept).unwrap(), original);
            assert_eq!(lines.len(), 1);
            assert!(
                lines[0].starts_with(
                    "claude: refreshed 2 files from source, kept 2 merged, left 1 to the rescan, \
                     legacy store kept at "
                ),
                "{}",
                lines[0]
            );
            assert!(lines[0].contains(&kept.display().to_string()));

            let pc = read_store();
            assert!(!pc.legacy);
            // Raised from the source; a message the stored entry never had is not added.
            assert_eq!(
                outputs(&pc, &key(1)),
                [
                    ("msg_01FastCCCCCCCCCCCCCCCCC".into(), 90),
                    ("msg_01SharedBBBBBBBBBBBBBBB".into(), 120),
                    ("msg_01StreamAAAAAAAAAAAAAAA".into(), 410),
                ]
            );
            // Changed source: left for the rescan, which replaces the entry on its mtime.
            assert_eq!(pc.files[&key(4)].mtime_secs, 1_737_000_000);
            assert_eq!(pc.files[&key(4)].records, before[&key(4)]);
            // Ceiling-trimmed aggregates: re-parsed, but nothing the aggregates counted comes back.
            assert_eq!(pc.files[&key(5)].records, before[&key(5)]);
            assert_eq!(
                pc.files[&key(5)]
                    .records
                    .iter()
                    .map(|r| r.output_tokens)
                    .sum::<u64>(),
                66
            );
            // Orphans stay as merged.
            for n in [2, 3] {
                assert_eq!(pc.files[&key(n)].records, before[&key(n)]);
            }

            // Rerun is a no-op.
            let bytes = fs::read(store()).unwrap();
            assert!(upgrade_stores(&quiet(), Duration::ZERO).unwrap().is_empty());
            assert_eq!(fs::read(store()).unwrap(), bytes);
            assert!(!with_suffix(&kept, ".1").exists());
            assert_eq!(fs::read(&kept).unwrap(), original);
        });
    }

    #[test]
    fn refresh_keeps_the_final_line_of_a_mixed_split_whole() {
        let split = |input, cache_read, out| UsageRecord {
            input_tokens: input,
            cache_read_input_tokens: cache_read,
            ..rec("m1", out)
        };
        let mut stored = vec![split(2, 107_292, 8)];
        raise(
            &mut stored,
            vec![
                split(2, 107_292, 8),
                split(3535, 97_739, 2075),
                split(2, 107_292, 8),
            ],
        );
        assert_eq!(
            (
                stored[0].input_tokens,
                stored[0].cache_read_input_tokens,
                stored[0].output_tokens
            ),
            (3535, 97_739, 2075)
        );
    }

    #[test]
    fn quarantined_native_store_is_folded_back() {
        with_scratch_home("upgrade-quarantine", || {
            write_legacy(&[
                ("/x/a.jsonl", 10, vec![rec("m1", 1)]),
                ("/x/b.jsonl", 10, vec![rec("m2", 2)]),
            ]);
            let data = store().parent().unwrap().to_path_buf();
            let native = data.join("claude.bin.quarantine-20260101000000");
            let q = cache(
                &[
                    ("/x/b.jsonl", 20, vec![rec("m3", 3)]),
                    ("/x/c.jsonl", 5, vec![rec("m4", 4)]),
                ],
                false,
            );
            fs::write(&native, format::encode(&q)).unwrap();
            let old = data.join("claude.bin.quarantine-20250101000000");
            fs::write(&old, b"older garbage").unwrap();

            assert!(pending());
            let lines = upgrade_stores(&quiet(), Duration::ZERO).unwrap();
            assert!(
                lines[0].contains("folded 1 quarantined store(s) back in"),
                "{}",
                lines[0]
            );
            assert!(!pending());
            assert!(!native.exists());
            assert_eq!(fs::read(&old).unwrap(), b"older garbage");
            let pc = read_store();
            assert_eq!(outputs(&pc, "/x/a.jsonl"), [("m1".into(), 1)]);
            assert_eq!(outputs(&pc, "/x/b.jsonl"), [("m3".into(), 3)]);
            assert_eq!(outputs(&pc, "/x/c.jsonl"), [("m4".into(), 4)]);

            // An older binary quarantining the native store again.
            let again = data.join("claude.bin.quarantine-20270101000000");
            fs::write(
                &again,
                format::encode(&cache(&[("/x/d.jsonl", 1, vec![rec("m5", 5)])], false)),
            )
            .unwrap();
            assert!(pending());
            upgrade_stores(&quiet(), Duration::ZERO).unwrap();
            assert!(!pending());
            assert!(!again.exists());
            assert_eq!(read_store().files.len(), 4);
        });
    }

    #[test]
    fn undecodable_native_quarantine_is_set_aside() {
        with_scratch_home("upgrade-unreadable", || {
            let data = paths::data_dir().unwrap();
            fs::create_dir_all(&data).unwrap();
            let mut bad = format::encode(&cache(&[("/x/a.jsonl", 1, vec![rec("m1", 1)])], false));
            bad.truncate(format::HEADER_LEN + 2);
            let q = data.join("claude.bin.quarantine-20260101000000");
            fs::write(&q, &bad).unwrap();

            assert!(pending());
            assert!(upgrade_stores(&quiet(), Duration::ZERO).unwrap().is_empty());
            assert!(!pending());
            assert_eq!(
                fs::read(data.join("claude.bin.unreadable-20260101000000")).unwrap(),
                bad
            );
        });
    }

    #[test]
    fn garbage_store_is_quarantined() {
        with_scratch_home("upgrade-garbage", || {
            fs::create_dir_all(store().parent().unwrap()).unwrap();
            fs::write(store(), b"not a store").unwrap();

            assert!(pending());
            let lines = upgrade_stores(&quiet(), Duration::ZERO).unwrap();
            assert_eq!(lines, ["claude: unusable store quarantined"]);
            assert!(!pending());
            assert!(!store().exists());
            let aside: Vec<_> = fs::read_dir(store().parent().unwrap())
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("claude.bin.quarantine-")
                })
                .collect();
            assert_eq!(aside.len(), 1);
            assert_eq!(fs::read(aside[0].path()).unwrap(), b"not a store");
        });
    }

    #[test]
    fn failed_native_write_leaves_the_store_legacy_and_pending() {
        with_scratch_home("upgrade-write-fails", || {
            let original = write_legacy(&[("/x/a.jsonl", 10, vec![rec("m1", 1)])]);
            // Occupies `atomic_write`'s temp path, so the native write fails after `.legacy` exists.
            let tmp = with_suffix(&store(), &format!(".{}.tmp", std::process::id()));
            fs::create_dir(&tmp).unwrap();

            assert!(upgrade_stores(&quiet(), Duration::ZERO).is_err());
            assert_eq!(fs::read(store()).unwrap(), original);
            assert!(pending());
            let kept = with_suffix(&store(), ".legacy");
            assert_eq!(fs::read(&kept).unwrap(), original);

            fs::remove_dir(&tmp).unwrap();
            upgrade_stores(&quiet(), Duration::ZERO).unwrap();
            assert!(!pending());
            assert!(is_native_on_disk());
            assert!(!with_suffix(&kept, ".1").exists());
        });
    }

    #[test]
    fn held_store_lock_leaves_the_store_legacy_and_flushes_stay_legacy() {
        with_scratch_home("upgrade-locked", || {
            write_legacy(&[("/x/a.jsonl", 10, vec![rec("m1", 1)])]);
            let lock = bitcode_store::try_lock_store(&store(), Duration::ZERO)
                .unwrap()
                .unwrap();
            let err = upgrade_stores(&quiet(), Duration::ZERO).unwrap_err();
            assert!(err.to_string().contains("store lock"), "{err:#}");
            drop(lock);
            assert!(!is_native_on_disk());
            assert!(pending());

            let mut storage = BitcodeStorage::new();
            storage.insert(
                "claude",
                Path::new("/x/new.jsonl"),
                1000,
                1,
                vec![rec("m2", 2)],
            );
            assert!(storage.flush());
            assert!(!is_native_on_disk());
            assert!(pending());
            let pc = read_store();
            assert!(pc.legacy);
            assert_eq!(outputs(&pc, "/x/a.jsonl"), [("m1".into(), 1)]);
            assert_eq!(outputs(&pc, "/x/new.jsonl"), [("m2".into(), 2)]);
        });
    }

    #[test]
    fn older_binary_rewrites_keep_a_single_legacy_backup() {
        with_scratch_home("upgrade-one-backup", || {
            let original = write_legacy(&[("/x/a.jsonl", 10, vec![rec("m1", 1)])]);
            upgrade_stores(&quiet(), Duration::ZERO).unwrap();
            let data = paths::data_dir().unwrap();
            for (n, stamp) in ["20270101000000", "20270102000000"].iter().enumerate() {
                // An older binary moves the native store aside and writes a fresh legacy one.
                fs::rename(store(), data.join(format!("claude.bin.quarantine-{stamp}"))).unwrap();
                write_legacy(&[(&format!("/x/new{n}.jsonl"), 20, vec![rec("m2", 2)])]);
                assert!(pending());
                let lines = upgrade_stores(&quiet(), Duration::ZERO).unwrap();
                assert!(!lines[0].contains("legacy store kept"), "{}", lines[0]);
                assert!(!pending());
            }
            let backups: Vec<_> = fs::read_dir(&data)
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("claude.bin.legacy")
                })
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(fs::read(backups[0].path()).unwrap(), original);
            assert_eq!(read_store().files.len(), 3);
        });
    }

    #[test]
    fn run_rescans_a_changed_source_into_the_native_store() {
        with_scratch_home("upgrade-run", || {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            let dir = home
                .join(".claude")
                .join("projects")
                .join("-home-u-git-app");
            fs::create_dir_all(&dir).unwrap();
            let source = dir.join("s1.jsonl");
            let key = source.to_string_lossy().into_owned();
            let ts = |secs: i64| {
                (chrono::Utc::now() - chrono::Duration::seconds(secs))
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string()
            };
            fs::write(
                &source,
                [
                    line("msg_a", "req_a", &ts(60), 3, 6),
                    line("msg_a", "req_a", &ts(59), 3, 410),
                    line("msg_b", "req_b", &ts(30), 1, 50),
                ]
                .join("\n")
                    + "\n",
            )
            .unwrap();
            write_legacy(&[(&key, 10, vec![rec("msg_a", 6)])]);

            let summary = run(&quiet()).unwrap();
            assert!(
                summary.starts_with(
                    "claude: refreshed 0 files from source, kept 0 merged, left 1 to the rescan, \
                     legacy store kept at "
                ),
                "{summary}"
            );
            assert!(is_native_on_disk());
            assert!(!pending());
            let pc = read_store();
            let df = crate::providers::discovered_file(&source).unwrap();
            assert_eq!(
                (pc.files[&key].mtime_secs, pc.files[&key].size),
                (df.mtime, df.size)
            );
            assert_eq!(
                outputs(&pc, &key),
                [("msg_a".into(), 410), ("msg_b".into(), 50)]
            );
        });
    }

    #[cfg(feature = "sqlite")]
    mod sqlite {
        use super::*;
        use crate::storage::sqlite_store::{self, SqliteStorage, V4_SCHEMA};
        use rusqlite::{params, Connection};

        fn db() -> PathBuf {
            paths::sqlite_db_file().unwrap()
        }

        fn ts(secs_ago: i64) -> String {
            (chrono::Utc::now() - chrono::Duration::seconds(secs_ago))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string()
        }

        /// (message id, request id, timestamp, output).
        type V4Row<'a> = (&'a str, &'a str, String, u64);

        /// A schema-4 database at the store path holding `files` as (path, mtime, size, rows).
        fn write_v4(files: &[(&str, i64, u64, Vec<V4Row>)]) {
            fs::create_dir_all(db().parent().unwrap()).unwrap();
            let conn = Connection::open(db()).unwrap();
            conn.execute_batch(V4_SCHEMA).unwrap();
            for (id, (path, mtime, size, rows)) in files.iter().enumerate() {
                conn.execute(
                    "INSERT INTO files VALUES (?1, 'claude', ?2, ?3, ?4)",
                    params![id as i64 + 1, path, mtime, *size as i64],
                )
                .unwrap();
                for (msg, req, ts, out) in rows {
                    conn.execute(
                        "INSERT INTO records VALUES
                             (?1, 's1', ?2, 'p', 'opus', ?3, ?4, 1, ?5, 0, 0, 0, 0, 'acct')",
                        params![id as i64 + 1, ts, msg, req, *out as i64],
                    )
                    .unwrap();
                }
            }
        }

        /// (path, message id, output, aggregate) of every row a report sees.
        fn report() -> Vec<(String, String, u64, bool)> {
            let mut store = SqliteStorage::open().unwrap();
            let conn = Connection::open(db()).unwrap();
            let paths: HashMap<String, String> = conn
                .prepare("SELECT r.message_id, f.path FROM records r JOIN files f USING (file_id)")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let mut out: Vec<_> = store
                .drain_all()
                .into_iter()
                .map(|r| {
                    let path = paths.get(&r.message_id).cloned().unwrap_or_default();
                    (path, r.message_id, r.output_tokens, r.aggregate)
                })
                .collect();
            out.sort();
            out
        }

        fn formats() -> Vec<i64> {
            let conn = Connection::open(db()).unwrap();
            let mut stmt = conn
                .prepare("SELECT format FROM files ORDER BY path")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        }

        #[test]
        fn upgrades_a_v4_database_end_to_end() {
            with_scratch_home("upgrade-sqlite", || {
                let home = PathBuf::from(std::env::var_os("HOME").unwrap());
                let dir = home.join(".claude/projects/-home-u-git-app");
                fs::create_dir_all(&dir).unwrap();
                let (s1, s2) = (dir.join("s1.jsonl"), dir.join("s2.jsonl"));
                let (t1, t2, t3) = (ts(60), ts(59), ts(30));
                fs::write(
                    &s1,
                    [
                        line("msg_a", "req_a", &t1, 3, 6),
                        line("msg_a", "req_a", &t2, 3, 410),
                        line("msg_b", "req_b", &t3, 1, 50),
                    ]
                    .join("\n")
                        + "\n",
                )
                .unwrap();
                fs::write(&s2, line("msg_c", "req_c", &t3, 1, 30) + "\n").unwrap();
                let df = crate::providers::discovered_file(&s1).unwrap();
                let day = chrono::Utc::now().date_naive();
                let legacy_agg = format!("tku-agg:s1:{day}:opus:acct:false");
                let (k1, k2) = (s1.to_string_lossy(), s2.to_string_lossy());
                write_v4(&[
                    (
                        &k1,
                        df.mtime,
                        df.size,
                        vec![
                            ("msg_a", "req_a", t1.clone(), 6),
                            ("msg_b", "req_b", t3.clone(), 50),
                        ],
                    ),
                    // Changed since it was stored.
                    (&k2, 10, 1, vec![("msg_c", "req_c", t3.clone(), 3)]),
                    (
                        "/gone/x.jsonl",
                        10,
                        1,
                        vec![
                            ("m1", "r1", t1.clone(), 6),
                            ("m1", "r1", t2.clone(), 6),
                            ("m1", "r1", t2.clone(), 410),
                            (&legacy_agg, "tku-agg", t1.clone(), 50),
                        ],
                    ),
                ]);

                assert!(pending());
                let summary = run(&quiet()).unwrap();
                assert!(
                    summary.starts_with(
                        "sqlite: refreshed 1 files from source, kept 1 merged, \
                         left 1 to the rescan, legacy database kept at "
                    ),
                    "{summary}"
                );
                assert!(!pending());
                assert_eq!(formats(), [2, 2, 2]);

                let kept = with_suffix(&db(), ".legacy");
                let legacy = Connection::open(&kept).unwrap();
                let version: i64 = legacy
                    .pragma_query_value(None, "user_version", |row| row.get(0))
                    .unwrap();
                assert_eq!(version, 4);
                let rows: i64 = legacy
                    .query_row("SELECT COUNT(*) FROM records", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(rows, 7);
                drop(legacy);

                let mut agg = UsageRecord {
                    session_id: "s1".into(),
                    timestamp: t1.parse().unwrap(),
                    model: "opus".into(),
                    account_uuid: Some("acct".into()),
                    ..rec("", 50)
                };
                agg.message_id = crate::types::aggregate_id(&agg);
                let gone = "/gone/x.jsonl".to_string();
                let mut expected = vec![
                    (k1.to_string(), "msg_a".into(), 410, false),
                    (k1.to_string(), "msg_b".into(), 50, false),
                    (k2.to_string(), "msg_c".into(), 30, false),
                    (gone.clone(), "m1".into(), 410, false),
                    (gone, agg.message_id, 50, true),
                ];
                expected.sort();
                assert_eq!(report(), expected);

                assert_eq!(run(&quiet()).unwrap(), "nothing to upgrade");
                assert!(!with_suffix(&kept, ".1").exists());
            });
        }

        #[test]
        fn resumes_a_partial_upgrade() {
            with_scratch_home("upgrade-sqlite-resume", || {
                let (t1, t2) = (ts(60), ts(59));
                write_v4(&[
                    (
                        "/gone/a.jsonl",
                        10,
                        1,
                        vec![("m1", "r1", t1.clone(), 6), ("m1", "r1", t2.clone(), 410)],
                    ),
                    (
                        "/gone/b.jsonl",
                        10,
                        1,
                        vec![("m2", "r2", t1.clone(), 6), ("m2", "r2", t2.clone(), 410)],
                    ),
                ]);
                // A run that kept the backup and finished `a` before it stopped.
                sqlite_store::backup(&db(), &with_suffix(&db(), ".legacy")).unwrap();
                drop(SqliteStorage::open().unwrap());
                Connection::open(db())
                    .unwrap()
                    .execute(
                        "UPDATE files SET format = 2 WHERE path = '/gone/a.jsonl'",
                        [],
                    )
                    .unwrap();

                assert!(pending());
                let summary = run(&quiet()).unwrap();
                assert_eq!(
                    summary,
                    "sqlite: refreshed 0 files from source, kept 1 merged, left 0 to the rescan"
                );
                assert!(!pending());
                assert_eq!(formats(), [2, 2]);
                let conn = Connection::open(db()).unwrap();
                let rows = |path: &str| -> i64 {
                    conn.query_row(
                        "SELECT COUNT(*) FROM records r JOIN files f USING (file_id) WHERE f.path = ?1",
                        params![path],
                        |row| row.get(0),
                    )
                    .unwrap()
                };
                assert_eq!(rows("/gone/a.jsonl"), 2);
                assert_eq!(rows("/gone/b.jsonl"), 1);
            });
        }

        #[test]
        fn a_missing_database_is_not_pending() {
            with_scratch_home("upgrade-sqlite-missing", || {
                assert!(!pending());
                assert!(!db().exists());
            });
        }
    }

    #[test]
    fn runs_between_relocate_and_account_ids() {
        let labels: Vec<&str> = crate::migrate::MIGRATIONS.iter().map(|m| m.label).collect();
        assert_eq!(labels, ["archive location", "store format", "account ids"]);
    }
}
