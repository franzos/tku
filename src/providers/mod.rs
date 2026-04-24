pub mod amp;
pub mod claude;
pub mod codex;
pub mod droid;
pub mod gemini;
pub mod kimi;
pub mod openclaw;
pub mod opencode;
pub mod pi;

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::accounts::redact;
use crate::storage::Storage;
use crate::types::UsageRecord;

/// Hard ceiling on whole-file size for JSONL sources. Legitimate session
/// transcripts don't approach this; anything larger is either a junk file
/// or a resource-exhaustion attempt. 500 MB matches the bitcode cap.
const MAX_FILE_BYTES: u64 = 500 * 1024 * 1024;

/// Per-line cap. JSONL lines that exceed this are skipped (not parsed),
/// which protects against a single pathological record from OOMing the
/// process. 16 MB is well above anything Claude/Codex generate in practice.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Progress callback throttle. On a warm cache we churn through tens of
/// thousands of files in well under a second; emitting a redraw per file
/// just floods the TTY with flushes. 50 ms / 32 files feels responsive
/// without the stutter.
const PROGRESS_THROTTLE_MS: u128 = 50;
const PROGRESS_THROTTLE_FILES: usize = 32;

pub trait Provider {
    /// Typed identity used on `UsageRecord.provider`.
    /// Prefer this over [`Self::name`] for matching/filtering; the string
    /// form is kept only for storage key plumbing that still threads `&str`.
    fn id(&self) -> crate::types::Provider;

    /// String form of the provider ID. Default impl defers to `id()` so new
    /// providers only need to implement `id()` + `root_dirs()` + `discover_and_parse()`.
    fn name(&self) -> &str {
        self.id().as_str()
    }

    fn root_dirs(&self) -> Vec<PathBuf>;
    fn discover_and_parse(
        &self,
        storage: &mut dyn Storage,
        progress: Option<&dyn Fn(usize, usize)>,
        prune: bool,
    );
}

/// Collect all provider root directories for file watching.
pub fn all_watch_paths() -> Vec<PathBuf> {
    all_providers()
        .iter()
        .flat_map(|p| p.root_dirs())
        .filter(|p| p.exists())
        .collect()
}

pub fn all_providers() -> Vec<Box<dyn Provider>> {
    vec![
        Box::new(claude::ClaudeProvider),
        Box::new(codex::CodexProvider),
        Box::new(pi::PiProvider),
        Box::new(amp::AmpProvider),
        Box::new(opencode::OpenCodeProvider),
        Box::new(gemini::GeminiProvider),
        Box::new(droid::DroidProvider),
        Box::new(openclaw::OpenClawProvider),
        Box::new(kimi::KimiProvider),
    ]
}

pub(crate) struct DiscoveredFile {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: u64,
}

pub(crate) fn discovered_file(path: &Path) -> Option<DiscoveredFile> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(DiscoveredFile {
        path: path.to_path_buf(),
        mtime,
        size: meta.len(),
    })
}

/// Walk `roots` once each and return every file whose extension (case-exact)
/// matches. Thin wrapper over [`discover_files_with`] for the common case.
pub(crate) fn discover_files(roots: &[PathBuf], extension: &str) -> Vec<DiscoveredFile> {
    discover_files_with(roots, |p| p.extension().is_some_and(|ext| ext == extension))
}

/// Walk `roots` once each and collect every file matching `accept`.
///
/// Callers that need finer control than a plain extension check should use
/// this — e.g. droid's `*.settings.json` (a compound suffix), or opencode's
/// single-walk classification where the same walk feeds `session/` and
/// `message/` subtrees from a shared parent root.
pub(crate) fn discover_files_with<F>(roots: &[PathBuf], accept: F) -> Vec<DiscoveredFile>
where
    F: Fn(&Path) -> bool,
{
    let mut files = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if accept(entry.path()) {
                if let Some(df) = discovered_file(entry.path()) {
                    files.push(df);
                }
            }
        }
    }

    files
}

pub(crate) fn discover_and_parse_with<F>(
    name: &str,
    files: Vec<DiscoveredFile>,
    storage: &mut dyn Storage,
    progress: Option<&dyn Fn(usize, usize)>,
    prune: bool,
    parse: F,
) where
    F: Fn(&Path) -> Vec<UsageRecord> + Sync,
{
    let total = files.len();

    // Throttled progress emitter — forwards to the caller's callback at most
    // every PROGRESS_THROTTLE_MS ms or PROGRESS_THROTTLE_FILES files, plus a
    // guaranteed final `total/total` so the UI never shows a stale number.
    let mut last_tick = Instant::now();
    let mut last_emitted: usize = 0;
    let mut emit = |current: usize, total: usize| {
        if let Some(cb) = &progress {
            let time_due = last_tick.elapsed().as_millis() >= PROGRESS_THROTTLE_MS;
            let count_due = current.saturating_sub(last_emitted) >= PROGRESS_THROTTLE_FILES;
            let is_final = current >= total;
            if time_due || count_due || is_final {
                cb(current, total);
                last_tick = Instant::now();
                last_emitted = current;
            }
        }
    };

    // Phase 1: filter out cached files (sequential — needs &mut storage)
    let mut cached_count = 0;
    let mut uncached: Vec<&DiscoveredFile> = Vec::new();
    for file in &files {
        if storage.is_cached(name, &file.path, file.mtime, file.size) {
            cached_count += 1;
            emit(cached_count, total);
        } else {
            uncached.push(file);
        }
    }

    // Phase 2: parse uncached files in parallel
    let results: Vec<_> = uncached
        .par_iter()
        .map(|file| (*file, parse(&file.path)))
        .collect();

    // Phase 3: insert results (sequential — needs &mut storage)
    for (i, (file, records)) in results.into_iter().enumerate() {
        emit(cached_count + i + 1, total);
        storage.insert(name, &file.path, file.mtime, file.size, records);
    }

    if prune {
        let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        storage.prune(name, &paths);
    }
}

/// XDG base directory kind, determining which env var and fallback to use.
pub(crate) enum XdgBase {
    /// Uses XDG_CONFIG_HOME, falls back to ~/.config
    Config,
    /// Uses XDG_DATA_HOME, falls back to ~/.local/share
    Data,
    /// Direct ~/.<name> path (legacy tool defaults)
    Home,
}

/// A home-relative fallback path: XDG base kind + subpath segments to join.
pub(crate) struct HomeFallback {
    pub base: XdgBase,
    pub subpaths: &'static [&'static str],
}

/// Compute provider root directories using a common pattern:
///
/// 1. If `env_var` is set, use its value joined with each of `env_subpaths`
/// 2. Otherwise, for each `HomeFallback`, resolve the XDG base directory
///    and join its subpath segments
///
/// Returns an empty Vec if neither the env var nor HOME is available.
pub(crate) fn compute_provider_roots(
    env_var: Option<&str>,
    env_subpaths: &[&str],
    home_fallbacks: &[HomeFallback],
) -> Vec<PathBuf> {
    // If the provider-specific env var is set, use it directly
    if let Some(var_name) = env_var {
        if let Ok(val) = std::env::var(var_name) {
            let base = PathBuf::from(val);
            if env_subpaths.is_empty() {
                return vec![base];
            }
            return env_subpaths.iter().map(|sub| base.join(sub)).collect();
        }
    }

    let home = match std::env::var_os("HOME").map(PathBuf::from) {
        Some(h) => h,
        None => return Vec::new(),
    };

    home_fallbacks
        .iter()
        .map(|fb| {
            let base = match fb.base {
                XdgBase::Config => std::env::var("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home.join(".config")),
                XdgBase::Data => std::env::var("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| home.join(".local").join("share")),
                XdgBase::Home => home.clone(),
            };
            let mut path = base;
            for seg in fb.subpaths {
                path = path.join(seg);
            }
            path
        })
        .collect()
}

/// Parse a JSONL file, filtering and extracting records with a common pattern:
///
/// 1. Skip whole file if it exceeds `MAX_FILE_BYTES` (DoS guard).
/// 2. Open file with BufReader.
/// 3. Read lines with a per-line byte cap; skip (don't truncate) oversize lines.
/// 4. For each line, check if it contains `filter` (fast pre-filter).
/// 5. If it passes, call `extract` with the raw line.
/// 6. Collect all Some results.
///
/// Returns an empty Vec on file open failure or oversized file.
pub(crate) fn parse_jsonl_lines<F, T>(path: &Path, filter: &str, extract: F) -> Vec<T>
where
    F: Fn(&str) -> Option<T>,
{
    // Whole-file guard: a file too big to be a legitimate session transcript
    // is almost certainly junk or hostile. Skip before opening.
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_FILE_BYTES {
            eprintln!("skipping oversize file: {}", redact(path));
            return Vec::new();
        }
    }

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let mut reader = BufReader::new(file);
    let mut results = Vec::new();
    let mut line = String::new();

    loop {
        line.clear();
        // Cap the bytes we'll absorb for a single line. `Take` limits the
        // inner reader so a single line with no newline can't blow out
        // memory.
        let mut limited = reader.by_ref().take(MAX_LINE_BYTES as u64 + 1);
        match limited.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(n) if n > MAX_LINE_BYTES => {
                // Line exceeded the cap. Drain the rest of the physical line
                // so we resume parsing at the next '\n' boundary instead of
                // mid-record on subsequent iterations.
                let mut sink = Vec::new();
                let mut drain = reader.by_ref().take(MAX_FILE_BYTES);
                // Read until newline or EOF; ignore errors (best-effort).
                let _ = drain.read_until(b'\n', &mut sink);
                continue;
            }
            Ok(_) => {}
            Err(_) => continue,
        }

        if !line.contains(filter) {
            continue;
        }

        if let Some(item) = extract(&line) {
            results.push(item);
        }
    }

    results
}
