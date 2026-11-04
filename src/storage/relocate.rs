//! Moves the usage archive from the cache dir to the data dir.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::bitcode_store::{self, Changes, ProviderCache};
use super::upgrade;
use crate::atomic_write::{atomic_write, sync_dir};
use crate::paths;
use crate::progress::Progress;

/// Runs before `ACCOUNT_IDS`, which rekeys the store at its current path.
pub const RELOCATE: crate::migrate::Migration = crate::migrate::Migration {
    label: "archive location",
    pending,
    run,
};

const DB: &str = "records.db";

/// Sources are renamed to this before reading, so an older tku's flush creates a new stray.
const RELOCATING: &str = ".relocating";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Store,
    Quarantine,
    Snapshots,
    Db,
}

fn kind(name: &str) -> Option<Kind> {
    if let Some(base) = name.strip_suffix(RELOCATING) {
        return kind(base).filter(|k| matches!(k, Kind::Store | Kind::Snapshots));
    }
    if name == DB {
        Some(Kind::Db)
    } else if name.ends_with(".bin") {
        Some(Kind::Store)
    } else if name.contains(".bin.quarantine-") || name.starts_with("records.db.quarantine-") {
        Some(Kind::Quarantine)
    } else if name.starts_with("subscription-") && name.ends_with(".json") {
        Some(Kind::Snapshots)
    } else {
        None
    }
}

fn dirs() -> Option<(PathBuf, PathBuf)> {
    let (cache, data) = (paths::cache_dir()?, paths::data_dir()?);
    (cache != data).then_some((cache, data))
}

/// Destination names to process, a leftover `<name>.relocating` counted under `<name>`.
fn entries(cache: &Path) -> BTreeSet<(String, Kind)> {
    let Ok(dir) = fs::read_dir(cache) else {
        return BTreeSet::new();
    };
    dir.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let kind = kind(&name)?;
            let base = name.strip_suffix(RELOCATING).unwrap_or(&name).to_string();
            Some((base, kind))
        })
        .collect()
}

fn pending() -> bool {
    dirs().is_some_and(|(cache, _)| !entries(&cache).is_empty())
}

fn run(progress: &Progress) -> Result<String> {
    let (cache, data) = dirs().ok_or_else(|| anyhow!("no cache or data dir"))?;
    fs::create_dir_all(&data).with_context(|| format!("create {}", data.display()))?;
    let files = entries(&cache);
    let (mut moved, mut kept, mut bytes, mut left) = (Vec::new(), Vec::new(), 0, 0);
    for (i, (name, kind)) in files.iter().enumerate() {
        progress.count("Moving the usage archive", i + 1, files.len());
        let (src, dst) = (cache.join(name), data.join(name));
        let outcome = match kind {
            Kind::Store => {
                let provider = name.trim_end_matches(".bin");
                relocate_store(&src, &dst, provider, progress).map(Some)
            }
            Kind::Quarantine => move_file(&src, &free_name(&dst))
                .map(Some)
                .map_err(anyhow::Error::from),
            Kind::Snapshots => via_aside(&src, |from| relocate_snapshots(from, &dst)).map(Some),
            Kind::Db => relocate_db(&src, &dst),
        };
        match outcome {
            Ok(Some(size)) => {
                moved.push(name.as_str());
                bytes += size;
            }
            Ok(None) => kept.push(name.as_str()),
            Err(e) => {
                eprintln!("tku: warning: {} left in place: {e:#}", src.display());
                left += 1;
            }
        }
    }
    if left > 0 {
        bail!("{left} file(s) still in {}", cache.display());
    }
    let mut summary = Vec::new();
    if !moved.is_empty() {
        summary.push(format!(
            "moved {} ({}) to {}",
            moved.join(", "),
            bitcode_store::human(bytes),
            data.display()
        ));
    }
    if !kept.is_empty() {
        summary.push(format!("left {} in {}", kept.join(", "), cache.display()));
    }
    if summary.is_empty() {
        return Ok("nothing to move".to_string());
    }
    Ok(summary.join("; "))
}

pub(super) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// `step` on a leftover `<src>.relocating`, then on `src` renamed to it. Returns the bytes taken.
fn via_aside(src: &Path, mut step: impl FnMut(&Path) -> Result<u64>) -> Result<u64> {
    let aside = with_suffix(src, RELOCATING);
    let mut size = 0;
    if aside.exists() {
        size += step(&aside)?;
    }
    if src.exists() {
        fs::rename(src, &aside).with_context(|| format!("rename to {}", aside.display()))?;
        size += step(&aside)?;
    }
    Ok(size)
}

fn relocate_store(src: &Path, dst: &Path, provider: &str, progress: &Progress) -> Result<u64> {
    // Held for the rename too: a flush between its read and write would overwrite the moved file.
    let _lock =
        bitcode_store::lock_store(dst, provider).ok_or_else(|| anyhow!("store lock not taken"))?;
    via_aside(src, |from| merge_store(from, dst, provider, progress))
}

/// A legacy side is upgraded after the merge and its bytes kept under a free `.legacy` name.
fn merge_store(src: &Path, dst: &Path, provider: &str, progress: &Progress) -> Result<u64> {
    if !dst.exists() {
        return Ok(move_file(src, dst)?);
    }
    let max_bytes = bitcode_store::configured_max_bytes();
    let dest = bitcode_store::read_provider(dst, max_bytes, false)
        .with_context(|| format!("cannot read {}, not overwriting it", dst.display()))?;
    let src_cache = match bitcode_store::read_provider(src, max_bytes, false) {
        Ok(pc) => pc,
        // Unusable for this build: kept whole beside the store rather than merged.
        Err(e) if e.kind() == io::ErrorKind::Other => {
            let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let aside = free_name(&dst.with_extension(format!("bin.quarantine-{stamp}")));
            let size = move_file(src, &aside)?;
            eprintln!(
                "tku: warning: {} is unusable ({e}); kept at {} instead of merging it",
                src.display(),
                aside.display()
            );
            return Ok(size);
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", src.display())),
    };
    let size = fs::metadata(src)?.len();
    let is_legacy = |pc: Option<&ProviderCache>| pc.is_some_and(|pc| pc.legacy);
    let (src_legacy, dest_legacy) = (is_legacy(src_cache.as_ref()), is_legacy(dest.as_ref()));
    let mut merged = src_cache.unwrap_or_default();
    let changes = Changes::touching_all(&merged);
    bitcode_store::merge_from_disk(&mut merged, dest.unwrap_or_default(), &changes);
    if src_legacy || dest_legacy {
        upgrade::upgrade_cache(provider, &mut merged, progress);
    }
    let data = bitcode_store::serialize_within(&mut merged, max_bytes, provider, &mut Vec::new())
        .ok_or_else(|| anyhow!("merged {provider} store not serialized"))?;
    let legacy_name = || free_name(&with_suffix(dst, ".legacy"));
    if dest_legacy {
        upgrade::preserve(dst, &legacy_name()).context("keep the legacy store")?;
    }
    atomic_write(dst, &data, None).with_context(|| format!("write {}", dst.display()))?;
    if src_legacy {
        sync_dir(dst.parent().unwrap_or(Path::new(".")))?;
        move_file(src, &legacy_name()).context("keep the legacy store")?;
    } else {
        remove_after(src, dst)?;
    }
    Ok(size)
}

fn relocate_snapshots(src: &Path, dst: &Path) -> Result<u64> {
    if !dst.exists() {
        return Ok(move_file(src, dst)?);
    }
    let size = fs::metadata(src)?.len();
    let merged = crate::subscription::merge_store_files(
        &fs::read_to_string(dst).with_context(|| format!("read {}", dst.display()))?,
        &fs::read_to_string(src)?,
    )?;
    atomic_write(dst, merged.as_bytes(), Some(0o600))
        .with_context(|| format!("write {}", dst.display()))?;
    remove_after(src, dst)?;
    Ok(size)
}

/// `None` when `dst` already exists and `src` stays where it is.
fn relocate_db(src: &Path, dst: &Path) -> Result<Option<u64>> {
    if dst.exists() {
        eprintln!(
            "tku: warning: {} was not moved because {} exists. Import its records with \
             `tku cache import-sqlite --db {}`, then remove {} and its -wal/-shm files.",
            src.display(),
            dst.display(),
            src.display(),
            src.display()
        );
        return Ok(None);
    }
    #[cfg(feature = "sqlite")]
    checkpoint(src)?;
    let mut size = 0;
    // Main file last: the migration stays pending until it has left the cache dir.
    for suffix in ["-wal", "-shm"] {
        let from = with_suffix(src, suffix);
        if from.exists() {
            size += move_file(&from, &with_suffix(dst, suffix))?;
        }
    }
    Ok(Some(size + move_file(src, dst)?))
}

#[cfg(feature = "sqlite")]
fn checkpoint(db: &Path) -> Result<()> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .with_context(|| format!("open {}", db.display()))?;
    let busy: i64 = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
        .context("checkpoint the WAL")?;
    if busy != 0 {
        bail!("database in use, WAL not checkpointed");
    }
    conn.close()
        .map_err(|(_, e)| e)
        .context("close the database")
}

/// `path`, or `path.1`, `path.2`, ... whichever is free first.
fn free_name(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    let mut n = 0u32;
    while candidate.exists() {
        n += 1;
        candidate = with_suffix(path, &format!(".{n}"));
    }
    candidate
}

/// Removes `src` once the directory holding `dst` is synced.
fn remove_after(src: &Path, dst: &Path) -> io::Result<()> {
    sync_dir(dst.parent().unwrap_or(Path::new(".")))?;
    fs::remove_file(src)
}

/// Rename, or copy and remove across filesystems. Returns the bytes moved.
fn move_file(src: &Path, dst: &Path) -> io::Result<u64> {
    let size = fs::metadata(src)?.len();
    match fs::rename(src, dst) {
        Ok(()) => sync_dir(dst.parent().unwrap_or(Path::new("."))).map(|()| size),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_across(src, dst)?;
            Ok(size)
        }
        Err(e) => Err(e),
    }
}

fn copy_across(src: &Path, dst: &Path) -> io::Result<()> {
    let data = fs::read(src)?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        Some(fs::metadata(src)?.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = None;
    atomic_write(dst, &data, mode)?;
    remove_after(src, dst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::bitcode_store::BitcodeStorage;
    use crate::storage::Storage;

    fn cache() -> PathBuf {
        let dir = paths::cache_dir().unwrap();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn data() -> PathBuf {
        paths::data_dir().unwrap()
    }

    fn record(account: &str) -> crate::types::UsageRecord {
        crate::types::UsageRecord {
            provider: crate::types::Provider::Claude,
            session_id: "s".into(),
            timestamp: chrono::Utc::now(),
            project: "p".into(),
            model: "opus".into(),
            message_id: "m".into(),
            request_id: "r".into(),
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: false,
            account_uuid: Some(account.into()),
            aggregate: false,
        }
    }

    /// Writes `data/claude.bin` holding `(key, mtime)` entries.
    fn write_store(entries: &[(&str, i64)]) {
        let mut store = BitcodeStorage::new();
        for (key, mtime) in entries {
            store.insert("claude", Path::new(key), *mtime, 0, vec![record("a")]);
        }
        assert!(store.flush());
    }

    fn run_once() -> String {
        run(&Progress::new(false)).unwrap()
    }

    #[test]
    fn moves_store_and_snapshots_byte_identical() {
        super::super::with_scratch_home("relocate-move", || {
            let cache = cache();
            fs::write(cache.join("claude.bin"), b"store bytes").unwrap();
            fs::write(cache.join("subscription-claude.json"), b"{\"version\":2}").unwrap();
            fs::write(cache.join("pricing-litellm.json"), b"{}").unwrap();

            let summary = run_once();

            assert_eq!(fs::read(data().join("claude.bin")).unwrap(), b"store bytes");
            assert_eq!(
                fs::read(data().join("subscription-claude.json")).unwrap(),
                b"{\"version\":2}"
            );
            assert!(!cache.join("claude.bin").exists());
            assert!(!cache.join("subscription-claude.json").exists());
            assert!(cache.join("pricing-litellm.json").exists());
            assert!(summary.starts_with("moved claude.bin, subscription-claude.json ("));
        });
    }

    #[test]
    fn merges_store_into_existing_destination() {
        super::super::with_scratch_home("relocate-merge", || {
            write_store(&[
                ("/old.jsonl", 0),
                ("/dest-newer.jsonl", 1),
                ("/src-newer.jsonl", 5),
            ]);
            fs::rename(data().join("claude.bin"), cache().join("claude.bin")).unwrap();
            write_store(&[
                ("/new.jsonl", 0),
                ("/dest-newer.jsonl", 2),
                ("/src-newer.jsonl", 3),
            ]);

            run_once();

            assert!(!cache().join("claude.bin").exists());
            assert!(!cache().join("claude.bin.relocating").exists());
            let mut store = BitcodeStorage::new();
            assert!(store.is_cached("claude", Path::new("/old.jsonl"), 0, 0));
            assert!(store.is_cached("claude", Path::new("/new.jsonl"), 0, 0));
            assert!(store.is_cached("claude", Path::new("/dest-newer.jsonl"), 2, 0));
            assert!(store.is_cached("claude", Path::new("/src-newer.jsonl"), 5, 0));
        });
    }

    fn out(n: u64) -> crate::types::UsageRecord {
        crate::types::UsageRecord {
            output_tokens: n,
            ..record("a")
        }
    }

    /// Encodes one store of `(key, mtime, records, sealed)` entries, legacy or native.
    fn encode(
        files: Vec<(&str, i64, Vec<crate::types::UsageRecord>, Vec<u64>)>,
        legacy: bool,
    ) -> Vec<u8> {
        let mut pc = ProviderCache::default();
        for (key, mtime_secs, records, sealed) in files {
            let cf = bitcode_store::CachedFile {
                mtime_secs,
                size: 0,
                records,
                sealed,
            };
            pc.files.insert(key.into(), cf);
        }
        if legacy {
            super::super::legacy::encode(&pc).unwrap()
        } else {
            super::super::format::encode(&pc)
        }
    }

    /// Writes `src` to the cache dir and `dest` to the data dir, runs, and returns the merged store.
    fn relocate_pair(src: &[u8], dest: &[u8]) -> ProviderCache {
        fs::write(cache().join("claude.bin"), src).unwrap();
        fs::create_dir_all(data()).unwrap();
        fs::write(data().join("claude.bin"), dest).unwrap();

        run_once();

        assert!(!cache().join("claude.bin").exists());
        assert!(!cache().join("claude.bin.relocating").exists());
        assert!(!(super::super::upgrade::STORE_FORMAT.pending)());
        let bytes = fs::read(data().join("claude.bin")).unwrap();
        assert!(super::super::format::is_native(&bytes));
        super::super::format::decode(&bytes).unwrap()
    }

    fn legacy_backup(suffix: &str) -> Vec<u8> {
        fs::read(data().join(format!("claude.bin.legacy{suffix}"))).unwrap()
    }

    fn outputs(pc: &ProviderCache, key: &str) -> Vec<u64> {
        pc.files[key]
            .records
            .iter()
            .map(|r| r.output_tokens)
            .collect()
    }

    #[test]
    fn legacy_source_is_upgraded_into_a_native_destination_and_kept() {
        super::super::with_scratch_home("relocate-legacy-src", || {
            let src = encode(
                vec![("/src.jsonl", 0, vec![out(6), out(6), out(410)], vec![])],
                true,
            );
            let dest = encode(vec![("/dest.jsonl", 0, vec![out(1)], vec![7, 9])], false);

            let merged = relocate_pair(&src, &dest);

            assert_eq!(merged.files["/dest.jsonl"].sealed, [7, 9]);
            assert_eq!(outputs(&merged, "/src.jsonl"), [410]);
            assert_eq!(legacy_backup(""), src);
            assert!(!data().join("claude.bin.legacy.1").exists());
        });
    }

    #[test]
    fn legacy_destination_is_upgraded_and_kept() {
        super::super::with_scratch_home("relocate-legacy-dest", || {
            let src = encode(vec![("/src.jsonl", 0, vec![out(1)], vec![3])], false);
            let dest = encode(
                vec![("/dest.jsonl", 0, vec![out(6), out(410)], vec![])],
                true,
            );

            let merged = relocate_pair(&src, &dest);

            assert_eq!(merged.files["/src.jsonl"].sealed, [3]);
            assert_eq!(outputs(&merged, "/dest.jsonl"), [410]);
            assert_eq!(legacy_backup(""), dest);
            assert!(!data().join("claude.bin.legacy.1").exists());
        });
    }

    #[test]
    fn both_legacy_sides_are_kept_under_free_names() {
        super::super::with_scratch_home("relocate-legacy-both", || {
            let src = encode(
                vec![("/src.jsonl", 0, vec![out(6), out(410)], vec![])],
                true,
            );
            let dest = encode(vec![("/dest.jsonl", 0, vec![out(2)], vec![])], true);

            let merged = relocate_pair(&src, &dest);

            assert_eq!(outputs(&merged, "/src.jsonl"), [410]);
            assert_eq!(outputs(&merged, "/dest.jsonl"), [2]);
            assert_eq!(legacy_backup(""), dest);
            assert_eq!(legacy_backup(".1"), src);
        });
    }

    #[test]
    fn newer_destination_copy_survives_and_is_refreshed() {
        super::super::with_scratch_home("relocate-legacy-newer", || {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap());
            let dir = home
                .join(".claude")
                .join("projects")
                .join("-home-u-git-app");
            fs::create_dir_all(&dir).unwrap();
            let source = dir.join("s1.jsonl");
            let line = |out: u64| {
                format!(
                    r#"{{"type":"assistant","timestamp":"2026-10-01T09:00:00Z","requestId":"r","message":{{"id":"m","model":"claude-opus-4-1-20250805","usage":{{"input_tokens":1,"output_tokens":{out}}}}}}}"#
                )
            };
            fs::write(&source, format!("{}\n{}\n", line(6), line(410))).unwrap();
            let df = crate::providers::discovered_file(&source).unwrap();
            let key = source.to_string_lossy().into_owned();
            let stale = crate::types::UsageRecord {
                message_id: "stale".into(),
                ..out(999)
            };
            let src = encode(vec![(&key, 1, vec![stale], vec![])], true);
            let mut dest = ProviderCache::default();
            dest.files.insert(
                key.clone(),
                bitcode_store::CachedFile {
                    mtime_secs: df.mtime,
                    size: df.size,
                    records: vec![out(6)],
                    sealed: Vec::new(),
                },
            );
            let dest = super::super::legacy::encode(&dest).unwrap();

            let merged = relocate_pair(&src, &dest);

            assert_eq!(merged.files[&key].mtime_secs, df.mtime);
            assert_eq!(outputs(&merged, &key), [410]);
        });
    }

    #[test]
    fn leftover_relocating_file_is_merged() {
        super::super::with_scratch_home("relocate-leftover", || {
            write_store(&[("/aside.jsonl", 0)]);
            fs::rename(
                data().join("claude.bin"),
                cache().join("claude.bin.relocating"),
            )
            .unwrap();
            write_store(&[("/stray.jsonl", 0)]);
            fs::rename(data().join("claude.bin"), cache().join("claude.bin")).unwrap();
            write_store(&[("/dest.jsonl", 0)]);
            assert!(pending());

            run_once();

            assert!(!pending());
            assert!(!cache().join("claude.bin").exists());
            assert!(!cache().join("claude.bin.relocating").exists());
            let mut store = BitcodeStorage::new();
            for key in ["/aside.jsonl", "/stray.jsonl", "/dest.jsonl"] {
                assert!(store.is_cached("claude", Path::new(key), 0, 0), "{key}");
            }
        });
    }

    #[test]
    fn pending_clears_after_run() {
        super::super::with_scratch_home("relocate-pending", || {
            let cache = cache();
            assert!(!pending());
            for name in [
                "claude.bin",
                "claude.bin.quarantine-20260101000000",
                "records.db.quarantine-20260101000000",
                "subscription-claude.json",
            ] {
                fs::write(cache.join(name), name).unwrap();
            }
            fs::create_dir_all(data()).unwrap();
            fs::write(data().join("records.db.quarantine-20260101000000"), "taken").unwrap();
            assert!(pending());

            run_once();

            assert!(!pending());
            assert_eq!(
                fs::read_to_string(data().join("records.db.quarantine-20260101000000.1")).unwrap(),
                "records.db.quarantine-20260101000000"
            );
        });
    }

    #[test]
    fn copy_across_keeps_bytes_and_mode() {
        super::super::with_scratch_home("relocate-copy", || {
            let (src, dst) = (cache().join("claude.bin"), cache().join("copy.bin"));
            fs::write(&src, b"payload").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();
            }
            let mode = fs::metadata(&src).unwrap().permissions();

            copy_across(&src, &dst).unwrap();

            assert_eq!(fs::read(&dst).unwrap(), b"payload");
            assert!(!src.exists());
            assert_eq!(fs::metadata(&dst).unwrap().permissions(), mode);
        });
    }

    #[test]
    fn relocated_store_is_rekeyed_by_account_ids() {
        super::super::with_scratch_home("relocate-rekey", || {
            let registry = paths::registry_file("claude").unwrap();
            fs::create_dir_all(registry.parent().unwrap()).unwrap();
            fs::write(
                &registry,
                r#"{
                    "version": 1,
                    "accounts": [{
                        "name": "work",
                        "org_uuid": "org-1",
                        "added_at": "2025-01-01T00:00:00Z",
                        "last_used_at": "2025-01-02T00:00:00Z",
                        "oauth_account": { "accountUuid": "acct-1", "organizationUuid": "org-1" }
                    }],
                    "switch_log": [
                        { "at": "2025-01-01T00:00:00Z", "org_uuid": "org-1", "name": "work", "source": "bootstrap" }
                    ]
                }"#,
            )
            .unwrap();
            let mut store = super::super::default_storage();
            store.insert("claude", Path::new("/a.jsonl"), 0, 0, vec![record("org-1")]);
            assert!(store.flush());
            drop(store);
            for entry in fs::read_dir(data()).unwrap().flatten() {
                let name = entry.file_name();
                if entry.file_type().unwrap().is_file() && name != ".store.lock" {
                    fs::rename(entry.path(), cache().join(name)).unwrap();
                }
            }

            crate::migrate::run_all(
                crate::migrate::MIGRATIONS,
                std::time::Duration::from_secs(30),
                &Progress::new(false),
            );

            assert!(!cache().join("claude.bin").exists());
            assert!(!cache().join(DB).exists());
            let mut store = super::super::default_storage();
            store.is_cached("claude", Path::new("/a.jsonl"), 0, 0);
            let records = store.drain_all();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].account_uuid.as_deref(), Some("acct-1@org-1"));
        });
    }

    #[cfg(not(feature = "sqlite"))]
    fn seed_db(cache: &Path) {
        for name in [DB, "records.db-wal", "records.db-shm"] {
            fs::write(cache.join(name), name).unwrap();
        }
    }

    #[cfg(feature = "sqlite")]
    /// The connection is returned so the `-wal`/`-shm` files survive until the run.
    fn seed_db(cache: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(cache.join(DB)).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('row');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn moves_records_db_with_its_sidecars() {
        super::super::with_scratch_home("relocate-db", || {
            let cache = cache();
            #[cfg(feature = "sqlite")]
            let seed = seed_db(&cache);
            #[cfg(not(feature = "sqlite"))]
            seed_db(&cache);
            assert!(cache.join("records.db-wal").exists());
            assert!(pending());

            run_once();

            assert!(!pending());
            assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
            for name in ["records.db-wal", "records.db-shm"] {
                assert!(data().join(name).exists(), "{name}");
            }
            #[cfg(not(feature = "sqlite"))]
            for name in [DB, "records.db-wal", "records.db-shm"] {
                assert_eq!(fs::read_to_string(data().join(name)).unwrap(), name);
            }
            #[cfg(feature = "sqlite")]
            {
                drop(seed);
                let conn = rusqlite::Connection::open(data().join(DB)).unwrap();
                let v: String = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
                assert_eq!(v, "row");
            }
        });
    }

    #[test]
    fn conflicting_records_db_stays_pending() {
        super::super::with_scratch_home("relocate-db-conflict", || {
            let cache = cache();
            fs::create_dir_all(data()).unwrap();
            fs::write(cache.join(DB), "old").unwrap();
            fs::write(data().join(DB), "new").unwrap();

            let summary = run_once();

            assert!(pending());
            assert!(summary.starts_with("left records.db in "), "{summary}");
            assert_eq!(fs::read_to_string(cache.join(DB)).unwrap(), "old");
            assert_eq!(fs::read_to_string(data().join(DB)).unwrap(), "new");
        });
    }
}
