//! Find and redact secrets across local agent session stores.
//!
//! Two passes. Pass 1 discovers secret *values* by walking JSON string leaves
//! (never keys) and applying gated detectors. Pass 2 sweeps every byte-exact
//! occurrence of each confirmed value across every store — one edit typically
//! stores the same secret 4-6 times, in `originalFile`, `oldString`,
//! `newString`, `structuredPatch`, `tool_use.input` and the tool result, so
//! redaction has to key on the value rather than on the match site.

pub mod detect;
pub mod fingerprint;
pub mod redact;
pub mod report;
pub mod rewrite;
pub mod state;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::providers::MAX_FILE_BYTES;
use crate::scrub::detect::{Category, Detector, Engine};
use crate::scrub::rewrite::{Matcher, Secret};
use crate::scrub::state::ScrubState;

/// Extensions that are never text. Skipped so they don't inflate the
/// "unreadable" count with expected misses.
const BINARY_EXTS: &[&str] = &[
    "pdf", "bin", "png", "jpg", "jpeg", "gif", "webp", "zip", "gz", "xz", "zst", "wasm", "so",
    "xls", "xlsx", "db", "sqlite",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// One JSON object per line.
    Jsonl,
    /// A single JSON document.
    Json,
    /// Opaque text.
    PlainText,
}

#[derive(Debug, Clone, Copy)]
pub enum Accept {
    Ext(&'static [&'static str]),
    AnyFile,
}

#[derive(Debug, Clone)]
pub struct ScrubTarget {
    pub root: PathBuf,
    pub accept: Accept,
    pub label: &'static str,
}

impl ScrubTarget {
    pub fn jsonl(root: PathBuf, label: &'static str) -> Self {
        Self {
            root,
            accept: Accept::Ext(&["jsonl"]),
            label,
        }
    }

    pub fn json(root: PathBuf, label: &'static str) -> Self {
        Self {
            root,
            accept: Accept::Ext(&["json"]),
            label,
        }
    }

    pub fn any(root: PathBuf, label: &'static str) -> Self {
        Self {
            root,
            accept: Accept::AnyFile,
            label,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScrubFile {
    pub path: PathBuf,
    pub kind: TargetKind,
    pub label: &'static str,
    /// Captured at discovery and re-checked immediately before the write.
    pub mtime: SystemTime,
    pub size: u64,
}

fn kind_for(path: &Path) -> TargetKind {
    match path.extension().and_then(|e| e.to_str()) {
        Some("jsonl") => TargetKind::Jsonl,
        Some("json") => TargetKind::Json,
        _ => TargetKind::PlainText,
    }
}

pub fn collect_targets() -> Vec<ScrubTarget> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for provider in crate::providers::all_providers() {
        for target in provider.scrub_targets() {
            if !target.root.exists() {
                continue;
            }
            // Claude resolves two roots (~/.claude and $XDG_CONFIG_HOME/claude);
            // without this the sibling stores are registered twice.
            let key = std::fs::canonicalize(&target.root).unwrap_or_else(|_| target.root.clone());
            if seen.insert(key) {
                out.push(target);
            }
        }
    }
    out
}

fn is_live(mtime: SystemTime, stale_after: Duration) -> bool {
    SystemTime::now()
        .duration_since(mtime)
        .map(|age| age < stale_after)
        .unwrap_or(true)
}

#[derive(Default)]
pub struct Discovery {
    pub files: Vec<ScrubFile>,
    pub live: Vec<PathBuf>,
    pub oversize: Vec<PathBuf>,
    pub symlinks: Vec<PathBuf>,
}

/// Phase, items done, items total. A total of 0 means "not yet known".
pub type Progress<'a> = &'a (dyn Fn(&str, usize, usize) + Sync);

/// Emit at most every this many items. The callback writes to a TTY, and the
/// scan hot path churns through thousands of files a second on a warm cache.
const PROGRESS_EVERY: usize = 64;

pub fn discover(
    targets: &[ScrubTarget],
    stale_after: Duration,
    progress: Option<Progress>,
) -> Discovery {
    let mut d = Discovery::default();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut walked = 0usize;

    for target in targets {
        for entry in WalkDir::new(&target.root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            walked += 1;
            if let Some(cb) = progress {
                if walked.is_multiple_of(PROGRESS_EVERY) {
                    cb("Finding files", walked, 0);
                }
            }

            // symlink_metadata, not is_file(): `~/.claude/debug/latest` is a
            // symlink, and renaming over it would destroy the link and leave
            // the real file unredacted.
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                d.symlinks.push(path.to_path_buf());
                continue;
            }
            if !meta.file_type().is_file() {
                continue;
            }

            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            match target.accept {
                Accept::Ext(exts) => {
                    if !exts.contains(&ext) {
                        continue;
                    }
                }
                Accept::AnyFile => {
                    if BINARY_EXTS.contains(&ext) {
                        continue;
                    }
                }
            }

            if !seen.insert(path.to_path_buf()) {
                continue;
            }
            if meta.len() > MAX_FILE_BYTES {
                d.oversize.push(path.to_path_buf());
                continue;
            }
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            if is_live(mtime, stale_after) {
                d.live.push(path.to_path_buf());
                continue;
            }

            d.files.push(ScrubFile {
                path: path.to_path_buf(),
                kind: kind_for(path),
                label: target.label,
                mtime,
                size: meta.len(),
            });
        }
    }

    d
}

// --- Pass 1 ---

fn scan_value(v: &serde_json::Value, engine: &Engine, out: &mut Vec<detect::Hit>) {
    match v {
        serde_json::Value::String(s) => engine.scan(s, out),
        serde_json::Value::Array(a) => a.iter().for_each(|x| scan_value(x, engine, out)),
        serde_json::Value::Object(o) => {
            for (key, val) in o {
                match val {
                    serde_json::Value::String(s) if detect::key_hints_at_credential(key) => {
                        // Re-scan as `key: value` so the contextual detectors see
                        // the label. Structured config puts it in the key, where
                        // a leaf-only scan can never reach it.
                        engine.scan(s, out);
                        engine.scan(&format!("{key}: {s}"), out);
                    }
                    other => scan_value(other, engine, out),
                }
            }
        }
        _ => {}
    }
}

fn collect_leaves<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match v {
        serde_json::Value::String(s) => out.push(s),
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_leaves(x, out)),
        serde_json::Value::Object(o) => o.values().for_each(|x| collect_leaves(x, out)),
        _ => {}
    }
}

/// Where a value sits on disk, kept only when `--preview` asked for it.
struct RawSite {
    path: PathBuf,
    label: &'static str,
    line: usize,
    /// On-disk text around the match, still containing it. Held at
    /// [`SCAN_WINDOW`] so the guard pass sees whole neighbouring values;
    /// masked and cut down to [`SHOW_WINDOW`] before it reaches the report.
    context: String,
}

/// Captured either side of the match. Wide enough that a value clipped at the
/// boundary — and therefore too short for the guard's length gate to
/// recognise — is far outside the text that actually gets displayed.
const SCAN_WINDOW: usize = 512;
/// Displayed either side of the match.
const SHOW_WINDOW: usize = 40;

fn clamp_left(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn clamp_right(s: &str, mut i: usize) -> usize {
    while i > 0 && i < s.len() && !s.is_char_boundary(i) {
        i -= 1;
    }
    i.min(s.len())
}

/// Is the occurrence at `off` a standalone token rather than the middle of a
/// longer one?
///
/// A short value can appear as a substring of an unrelated, longer credential —
/// a 15-character URL password inside a 32-character API key, say — and taking
/// the first `find` would then report a location the detector never matched at.
fn is_standalone(data: &str, off: usize, len: usize) -> bool {
    // `=` is deliberately absent: it is base64 padding but also the assignment
    // operator, and treating `KEY=value` as one token would reject every
    // environment-variable site.
    let credential_byte =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'_' | b'-');
    let before_ok = off == 0 || !credential_byte(data.as_bytes()[off - 1]);
    let after = off + len;
    let after_ok = after >= data.len() || !credential_byte(data.as_bytes()[after]);
    before_ok && after_ok
}

fn find_site_offset(data: &str, form: &str) -> Option<usize> {
    let mut from = 0;
    let mut fallback = None;
    while let Some(rel) = data[from..].find(form) {
        let off = from + rel;
        if is_standalone(data, off, form.len()) {
            return Some(off);
        }
        fallback.get_or_insert(off);
        from = off + form.len();
    }
    fallback
}

fn extract_site(data: &str, value: &str, path: &Path, label: &'static str) -> Option<RawSite> {
    let (off, len) = rewrite::encoded_forms(value)
        .into_iter()
        .find_map(|form| find_site_offset(data, &form).map(|o| (o, form.len())))?;

    let line_start = data[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = data[off..]
        .find('\n')
        .map(|i| off + i)
        .unwrap_or(data.len());

    let start = clamp_left(data, off.saturating_sub(SCAN_WINDOW).max(line_start));
    let end = clamp_right(data, (off + len + SCAN_WINDOW).min(line_end));

    Some(RawSite {
        path: path.to_path_buf(),
        label,
        line: data[..off].matches('\n').count() + 1,
        context: data[start..end].to_string(),
    })
}

/// Capture one site per value, from the *decoded* string leaf wherever the file
/// is JSON.
///
/// Decoding matters for more than readability. In a raw `.jsonl` line a newline
/// is the two bytes `\` `n`, so a detector's value capture runs straight through
/// it and swallows the following key too; the resulting blob contains a
/// backslash, trips the code-marker rejection, and the guard pass never sees the
/// neighbouring secret it was supposed to mask.
fn collect_sites(
    data: &str,
    file: &ScrubFile,
    values: &HashMap<String, (&'static Detector, usize)>,
    sites: &mut HashMap<String, RawSite>,
) {
    let take = |text: &str, line: usize, sites: &mut HashMap<String, RawSite>| {
        for value in values.keys() {
            // Standalone, not merely present: a leaf that happens to contain the
            // value inside a longer token is not where the detector matched.
            let standalone = rewrite::encoded_forms(value).iter().any(|f| {
                find_site_offset(text, f).is_some_and(|o| is_standalone(text, o, f.len()))
            });
            if sites.contains_key(value) || !standalone {
                continue;
            }
            if let Some(mut site) = extract_site(text, value, &file.path, file.label) {
                // 0 means "keep the line extract_site worked out", which is
                // correct only for plain text, where offsets are file offsets.
                if line != 0 {
                    site.line = line;
                }
                sites.insert(value.clone(), site);
            }
        }
    };

    match file.kind {
        TargetKind::Jsonl => {
            for (i, line) in data.lines().enumerate() {
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(v) => {
                        let mut leaves = Vec::new();
                        collect_leaves(&v, &mut leaves);
                        for leaf in leaves {
                            take(leaf, i + 1, sites);
                        }
                    }
                    Err(_) => take(line, i + 1, sites),
                }
            }
        }
        TargetKind::Json => match serde_json::from_str::<serde_json::Value>(data) {
            Ok(v) => {
                let mut leaves = Vec::new();
                collect_leaves(&v, &mut leaves);
                for leaf in leaves {
                    take(leaf, 1, sites);
                }
            }
            Err(_) => take(data, 1, sites),
        },
        TargetKind::PlainText => take(data, 0, sites),
    }
}

struct FileScan {
    /// value -> (detector, occurrences in this file)
    values: HashMap<String, (&'static Detector, usize)>,
    sites: HashMap<String, RawSite>,
    bytes: u64,
    unreadable: bool,
}

fn scan_file(file: &ScrubFile, engine: &Engine, preview: bool) -> FileScan {
    let Ok(data) = std::fs::read_to_string(&file.path) else {
        return FileScan {
            values: HashMap::new(),
            sites: HashMap::new(),
            bytes: 0,
            unreadable: true,
        };
    };
    let bytes = data.len() as u64;
    let mut hits = Vec::new();

    match file.kind {
        TargetKind::Jsonl => {
            for line in data.lines() {
                match serde_json::from_str::<serde_json::Value>(line) {
                    // String leaves only — scanning the raw line would let a
                    // detector fire on a JSON key.
                    Ok(v) => scan_value(&v, engine, &mut hits),
                    Err(_) => engine.scan(line, &mut hits),
                }
            }
        }
        TargetKind::Json => match serde_json::from_str::<serde_json::Value>(&data) {
            Ok(v) => scan_value(&v, engine, &mut hits),
            Err(_) => engine.scan(&data, &mut hits),
        },
        TargetKind::PlainText => engine.scan(&data, &mut hits),
    }

    let mut values: HashMap<String, (&'static Detector, usize)> = HashMap::new();
    for hit in hits {
        values
            .entry(hit.value)
            .and_modify(|e| e.1 += 1)
            .or_insert((hit.detector, 1));
    }

    // Guarded on `values`: collect_sites re-walks the file, and the
    // overwhelming majority of files have no hits at all.
    let mut sites = HashMap::new();
    if preview && !values.is_empty() {
        collect_sites(&data, file, &values, &mut sites);
    }

    FileScan {
        values,
        sites,
        bytes,
        unreadable: false,
    }
}

// --- Orchestration ---

pub struct Options {
    pub only: Option<Vec<String>>,
    pub skip: Vec<String>,
    pub apply: bool,
    pub stale_after: Duration,
    pub max_spread: usize,
    pub include_wide: bool,
    /// Sites to show per finding. 0 disables preview collection entirely.
    pub preview: usize,
    /// Write every detected secret, in plaintext, here.
    pub export: Option<PathBuf>,
}

/// Refuse to write the export inside a directory scrub itself scans.
///
/// Otherwise the next run detects the export, reports its own output as
/// findings, and — with `--apply` — redacts the undo map.
fn export_path_is_safe(path: &Path, targets: &[ScrubTarget]) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    export_path_is_safe_from(path, &cwd, targets)
}

/// `base` is the directory a relative `path` resolves against. Taking it as an
/// argument keeps the test off `set_current_dir`, which is process-global and
/// would race the parallel test runner.
fn export_path_is_safe_from(path: &Path, base: &Path, targets: &[ScrubTarget]) -> bool {
    // Resolve first. `Path::new("secrets.json").parent()` is `Some("")`, which
    // fails to canonicalize, and a relative path can never `starts_with` an
    // absolute root — so without this the guard silently passes everything,
    // including the bare filename the docs suggest.
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let abs = joined
        .parent()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .unwrap_or(joined);
    // Only containment matters: a file is scanned when it sits inside a root.
    // Writing to a parent of a root is fine.
    !targets.iter().any(|t| {
        std::fs::canonicalize(&t.root)
            .map(|root| abs.starts_with(&root))
            .unwrap_or(false)
    })
}

/// Write the plaintext secret inventory.
///
/// Doubles as an undo map: every entry pairs the value with the exact
/// replacement written into the transcripts, so restoring one is a search for
/// its `[REDACTED:…]` marker.
fn write_export(path: &Path, entries: &[(String, &Finding)], applied: bool) -> Result<()> {
    let secrets: Vec<serde_json::Value> = entries
        .iter()
        .map(|(value, f)| {
            serde_json::json!({
                "detector": f.detector_id,
                "fingerprint": f.fp,
                "full_fingerprint": f.full_fp,
                "value": value,
                "replacement": f.masked,
                "occurrences": f.occurrences,
                "files": f.files,
                "swept": !f.wide,
                "rotate": report::rotation_hint(f.detector_id),
            })
        })
        .collect();

    let doc = serde_json::json!({
        "note": "Plaintext secrets. Every entry pairs `value` with the `replacement` \
                 written into your transcripts, so this is also an undo map: search for \
                 the replacement, restore the value. Rotate these and delete this file.",
        "applied": applied,
        "count": secrets.len(),
        "secrets": secrets,
    });

    let data = serde_json::to_vec_pretty(&doc)?;
    crate::atomic_write::atomic_write(path, &data, Some(0o600))
        .with_context(|| format!("failed to write {}", path.display()))
}

/// One occurrence, rendered for display. Neither side contains the secret.
pub struct PreviewSite {
    pub path: PathBuf,
    pub label: &'static str,
    pub line: usize,
    pub before: String,
    pub after: String,
}

pub struct Finding {
    pub detector_id: &'static str,
    pub category: Category,
    pub fp: String,
    pub full_fp: String,
    pub masked: String,
    pub occurrences: usize,
    pub files: usize,
    /// Reported but excluded from the sweep for exceeding `--max-spread`.
    pub wide: bool,
    pub sites: Vec<PreviewSite>,
}

#[derive(Default)]
pub struct ScrubOutcome {
    pub findings: Vec<Finding>,
    pub files_scanned: usize,
    pub bytes_scanned: u64,
    pub files_rewritten: usize,
    pub applied: bool,
    pub live_skipped: Vec<PathBuf>,
    pub oversize_skipped: Vec<PathBuf>,
    pub symlinks_skipped: Vec<PathBuf>,
    pub unreadable: usize,
    pub failed: Vec<(PathBuf, String)>,
    /// Files scanned per store label, so the report can show that e.g.
    /// file-history was actually reached and not silently empty.
    pub per_store: BTreeMap<&'static str, usize>,
    /// Where the plaintext export was written, and how many secrets it holds.
    pub exported: Option<(PathBuf, usize)>,
}

struct ValueStat {
    detector: &'static Detector,
    occurrences: usize,
    files: usize,
}

/// How many files each candidate *appears* in, byte-exact.
///
/// The spread guard used to count the files a value was *detected* in, which is
/// a different and much smaller number. `portal` was detected as a URL password
/// in 4 files, passed the threshold, and the sweep then rewrote the word
/// "portal" in 169 files of ordinary prose. Detection spread says nothing about
/// blast radius; this does.
fn count_value_spread(
    files: &[ScrubFile],
    values: &[String],
    progress: Option<Progress>,
) -> HashMap<String, usize> {
    if values.is_empty() {
        return HashMap::new();
    }

    let mut patterns = Vec::new();
    let mut owner = Vec::new();
    for (i, v) in values.iter().enumerate() {
        for form in rewrite::encoded_forms(v) {
            if form.is_empty() {
                continue;
            }
            patterns.push(form);
            owner.push(i);
        }
    }
    let Ok(ac) = aho_corasick::AhoCorasick::new(&patterns) else {
        return HashMap::new();
    };

    let done = AtomicUsize::new(0);
    let total = files.len();
    let per_file: Vec<Vec<usize>> = files
        .par_iter()
        .map(|f| {
            let mut seen = vec![false; values.len()];
            if let Ok(data) = std::fs::read_to_string(&f.path) {
                for m in ac.find_iter(&data) {
                    seen[owner[m.pattern().as_usize()]] = true;
                }
            }
            if let Some(cb) = progress {
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(PROGRESS_EVERY) || n == total {
                    cb("Measuring spread", n, total);
                }
            }
            seen.iter()
                .enumerate()
                .filter_map(|(i, s)| s.then_some(i))
                .collect()
        })
        .collect();

    let mut counts = vec![0usize; values.len()];
    for hits in per_file {
        for i in hits {
            counts[i] += 1;
        }
    }
    values.iter().cloned().zip(counts).collect()
}

/// Scrub anything credential-shaped out of preview context.
///
/// The window around a match is raw on-disk text, and secrets cluster: an
/// `AWS_ACCESS_KEY_ID` line is usually followed by `AWS_SECRET_ACCESS_KEY`.
/// `guard` therefore runs the *full* default detector set, not the user's
/// `--only` selection, so narrowing the report can never widen the exposure.
fn sanitize_context(s: &str, guard: &Engine) -> String {
    let mut hits = Vec::new();
    guard.scan(s, &mut hits);
    let mut out = s.to_string();
    for hit in hits {
        for form in rewrite::encoded_forms(&hit.value) {
            out = out.replace(&form, "[…]");
        }
    }
    detect::mask_credential_runs(&out)
}

/// Replace the match with `marker`, mask every other secret in the window, and
/// only then cut down to display width.
///
/// Order matters. Cutting first would slice a neighbouring value in half, and a
/// half-length value falls under the detectors' length gate — so the guard pass
/// would not recognise it and the fragment would be printed. On a real corpus
/// that surfaced 16 characters of an `AWS_SECRET_ACCESS_KEY`.
fn render_one(context: &str, value: &str, marker: &str, guard: &Engine) -> String {
    let mut line = context.to_string();
    for form in rewrite::encoded_forms(value) {
        line = line.replace(&form, marker);
    }
    let line = sanitize_context(&line, guard).replace(['\n', '\r', '\t'], " ");

    let Some(at) = line.find(marker) else {
        return line;
    };
    let start = clamp_left(&line, at.saturating_sub(SHOW_WINDOW));
    let end = clamp_right(&line, (at + marker.len() + SHOW_WINDOW).min(line.len()));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&line[start..end]);
    if end < line.len() {
        out.push('…');
    }
    out
}

/// Render both sides of a site without ever printing a secret.
fn render_site(
    site: &RawSite,
    det: &'static Detector,
    value: &str,
    fp: &str,
    guard: &Engine,
) -> PreviewSite {
    PreviewSite {
        path: site.path.clone(),
        label: site.label,
        line: site.line,
        before: render_one(&site.context, value, &redact::preview(det, value), guard),
        after: render_one(&site.context, value, &redact::mask(det, value, fp), guard),
    }
}

pub fn run(opts: &Options, state: &ScrubState, progress: Option<Progress>) -> Result<ScrubOutcome> {
    let custom = detect::build_custom(&state.custom)?;
    let dets = detect::resolve(&custom, opts.only.as_deref(), &opts.skip)?;
    let engine = Engine::new(&dets)?;

    let targets = collect_targets();
    let discovery = discover(&targets, opts.stale_after, progress);
    let files = discovery.files;

    let mut outcome = ScrubOutcome {
        files_scanned: files.len(),
        applied: opts.apply,
        live_skipped: discovery.live,
        oversize_skipped: discovery.oversize,
        symlinks_skipped: discovery.symlinks,
        ..Default::default()
    };

    for f in &files {
        *outcome.per_store.entry(f.label).or_insert(0) += 1;
    }

    // Pass 1 — discover, parallel per file.
    let want_preview = opts.preview > 0;
    // Always the full default set, so `--only` can't widen preview exposure.
    let guard = if want_preview {
        Some(Engine::new(&detect::resolve(&custom, None, &[])?)?)
    } else {
        None
    };
    let scanned = AtomicUsize::new(0);
    let total = files.len();
    let scans: Vec<FileScan> = files
        .par_iter()
        .map(|f| {
            let out = scan_file(f, &engine, want_preview);
            if let Some(cb) = progress {
                let n = scanned.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(PROGRESS_EVERY) || n == total {
                    cb("Scanning", n, total);
                }
            }
            out
        })
        .collect();

    let allowed = state.allowed_set();
    let mut stats: BTreeMap<String, ValueStat> = BTreeMap::new();
    let mut raw_sites: HashMap<String, Vec<RawSite>> = HashMap::new();
    for mut scan in scans {
        outcome.bytes_scanned += scan.bytes;
        if scan.unreadable {
            outcome.unreadable += 1;
            continue;
        }
        for (value, site) in scan.sites.drain() {
            let slot = raw_sites.entry(value).or_default();
            if slot.len() < opts.preview {
                slot.push(site);
            }
        }
        for (value, (det, count)) in scan.values {
            if allowed.contains(state.fingerprint(&value).as_str()) {
                continue;
            }
            stats
                .entry(value)
                .and_modify(|s| {
                    s.occurrences += count;
                    s.files += 1;
                })
                .or_insert(ValueStat {
                    detector: det,
                    occurrences: count,
                    files: 1,
                });
        }
    }

    // A contextual hit spread across many files is a false positive — on the
    // reference corpus the wide ones were `grant_type…`, `kSecAttr…` and
    // `NOTARY…`. Vendor-prefixed hits carry their own confidence, so they are
    // never spread-limited.
    let spread_candidates: Vec<String> = stats
        .iter()
        .filter(|(_, s)| s.detector.spread_limited())
        .map(|(v, _)| v.clone())
        .collect();
    let corpus_spread = count_value_spread(&files, &spread_candidates, progress);

    let is_wide = |value: &str, s: &ValueStat| {
        s.detector.spread_limited()
            && corpus_spread.get(value).copied().unwrap_or(s.files) > opts.max_spread
            && !opts.include_wide
    };

    let mut findings: Vec<Finding> = Vec::with_capacity(stats.len());
    let mut plaintext: Vec<String> = Vec::with_capacity(stats.len());
    let mut secrets: Vec<Secret> = Vec::new();
    for (value, stat) in &stats {
        let full = state.fingerprint(value);
        let fp = fingerprint::short(&full).to_string();
        let wide = is_wide(value, stat);
        findings.push(Finding {
            detector_id: stat.detector.id,
            category: stat.detector.category,
            fp: fp.clone(),
            full_fp: full,
            masked: redact::mask(stat.detector, value, &fp),
            occurrences: stat.occurrences,
            files: corpus_spread
                .get(value)
                .copied()
                .unwrap_or(stat.files)
                .max(stat.files),
            wide,
            sites: match &guard {
                Some(g) => raw_sites
                    .remove(value)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| render_site(&s, stat.detector, value, &fp, g))
                    .collect(),
                None => Vec::new(),
            },
        });
        plaintext.push(value.clone());
        if !wide {
            secrets.push(Secret {
                value: value.clone(),
                detector: stat.detector,
                fp,
            });
        }
    }

    // Pass 2 — sweep. Needs the complete value set, so it is a real barrier.
    if opts.apply && !secrets.is_empty() {
        let matcher = Matcher::new(&secrets)?;
        let written = AtomicUsize::new(0);
        let results: Vec<(PathBuf, Result<bool>)> = files
            .par_iter()
            .map(|f| {
                let out = (f.path.clone(), rewrite_file(f, &matcher, opts.stale_after));
                if let Some(cb) = progress {
                    let n = written.fetch_add(1, Ordering::Relaxed) + 1;
                    if n.is_multiple_of(PROGRESS_EVERY) || n == total {
                        cb("Redacting", n, total);
                    }
                }
                out
            })
            .collect();
        for (path, res) in results {
            match res {
                Ok(true) => outcome.files_rewritten += 1,
                Ok(false) => {}
                Err(e) => outcome.failed.push((path, e.to_string())),
            }
        }
    }

    if let Some(path) = &opts.export {
        if !export_path_is_safe(path, &targets) {
            anyhow::bail!(
                "refusing to write {} inside a directory scrub scans: the next run would \
                 detect the export and, with --apply, redact your own undo map",
                path.display()
            );
        }
        // Paired at construction, so this cannot drift when `findings` is sorted.
        let entries: Vec<(String, &Finding)> =
            plaintext.iter().cloned().zip(findings.iter()).collect();
        write_export(path, &entries, opts.apply)?;
        outcome.exported = Some((path.clone(), entries.len()));
    }

    findings.sort_by(|a, b| {
        a.detector_id
            .cmp(b.detector_id)
            .then(b.occurrences.cmp(&a.occurrences))
    });
    outcome.findings = findings;

    Ok(outcome)
}

/// Returns true when the file was rewritten.
fn rewrite_file(file: &ScrubFile, matcher: &Matcher, stale_after: Duration) -> Result<bool> {
    let data = match std::fs::read_to_string(&file.path) {
        Ok(d) => d,
        // Already counted as unreadable during the scan; reporting it again as
        // a rewrite failure would double-count the same four files.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let jsonl = matches!(file.kind, TargetKind::Jsonl);
    let Some(out) = rewrite::rewrite_text(&data, matcher, jsonl)? else {
        return Ok(false);
    };

    // Liveness was sampled at discovery, potentially minutes ago. Re-check
    // against the file we just read: renaming over a transcript a live agent
    // holds open by fd silently discards everything it appended since.
    let meta = std::fs::symlink_metadata(&file.path)?;
    if !meta.file_type().is_file() {
        anyhow::bail!("no longer a regular file");
    }
    if meta.len() != file.size || meta.modified().ok() != Some(file.mtime) {
        anyhow::bail!("changed on disk during the scan");
    }
    if is_live(file.mtime, stale_after) {
        anyhow::bail!("became live during the scan");
    }

    let mode = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            Some(meta.permissions().mode() & 0o777)
        }
        #[cfg(not(unix))]
        {
            Some(0o600)
        }
    };

    // Re-checked once more immediately before the rename: the write and fsync
    // above take real time on a large transcript, and that is exactly the
    // window in which an idle session can wake up and append.
    let path = &file.path;
    let (size, mtime) = (meta.len(), meta.modified().ok());
    crate::atomic_write::atomic_write_checked(path, out.as_bytes(), mode, || {
        let now = std::fs::symlink_metadata(path)?;
        if now.len() != size || now.modified().ok() != mtime {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "changed on disk while being rewritten",
            ));
        }
        Ok(())
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::Provider as _;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("tku-scrub-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claude_targets_include_the_sibling_stores() {
        let labels: Vec<&str> = crate::providers::claude::ClaudeProvider
            .scrub_targets()
            .iter()
            .map(|t| t.label)
            .collect();
        assert!(labels.contains(&"transcripts"));
        assert!(labels.contains(&"prompt-history"));
        assert!(labels.contains(&"file-history"));
        assert!(labels.contains(&"config"));
    }

    #[test]
    fn codex_targets_its_jsonl_sessions() {
        // The provider reads ~/.codex/sessions/**.jsonl, so scrub must cover it
        // even though newer Codex builds have moved to sqlite.
        assert!(!crate::providers::codex::CodexProvider
            .scrub_targets()
            .is_empty());
    }

    #[test]
    fn kind_is_derived_from_the_extension() {
        assert_eq!(kind_for(Path::new("a/b.jsonl")), TargetKind::Jsonl);
        assert_eq!(kind_for(Path::new("a/b.json")), TargetKind::Json);
        assert_eq!(kind_for(Path::new("a/b.md")), TargetKind::PlainText);
        assert_eq!(kind_for(Path::new("a/b")), TargetKind::PlainText);
    }

    #[test]
    fn a_freshly_written_file_counts_as_live() {
        let now = SystemTime::now();
        assert!(is_live(now, Duration::from_secs(300)));
        assert!(!is_live(
            now - Duration::from_secs(600),
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn discovery_skips_symlinks() {
        let dir = scratch("symlink");
        std::fs::write(dir.join("real.jsonl"), "{}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("real.jsonl"), dir.join("link.jsonl")).unwrap();

        let targets = vec![ScrubTarget::jsonl(dir.clone(), "test")];
        let d = discover(&targets, Duration::from_secs(0), None);

        assert_eq!(d.files.len(), 1);
        assert!(d.files[0].path.ends_with("real.jsonl"));
        #[cfg(unix)]
        assert_eq!(d.symlinks.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_that_changes_after_discovery_is_not_rewritten() {
        const KEY: &str = "sk-or-v1-cdb75f9a2e1b4c8d6f0a3e5b7c9d1f2a4b6c8d0e2f4a6b8c";
        let dir = scratch("toctou");
        let path = dir.join("s.jsonl");
        std::fs::write(&path, format!("{{\"a\":\"{KEY}\"}}\n")).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let file = ScrubFile {
            path: path.clone(),
            kind: TargetKind::Jsonl,
            label: "test",
            mtime: meta.modified().unwrap(),
            size: meta.len(),
        };

        // Grow the file behind our back, as a live session would.
        std::fs::write(&path, format!("{{\"a\":\"{KEY}\"}}\n{{\"b\":2}}\n")).unwrap();

        let det = detect::DETECTORS
            .iter()
            .find(|d| d.id == "openrouter-key")
            .unwrap();
        let secrets = vec![Secret {
            value: KEY.to_string(),
            detector: det,
            fp: "a3f91c04".to_string(),
        }];
        let matcher = Matcher::new(&secrets).unwrap();

        assert!(rewrite_file(&file, &matcher, Duration::from_secs(0)).is_err());
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(KEY), "file must be left untouched");
        assert!(after.contains("\"b\":2"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preview_context_does_not_leak_a_neighbouring_secret() {
        // Regression: the window around an AWS key id runs straight into the
        // secret access key on the next line. Narrowing the report with --only
        // must not widen what the preview prints.
        const ID: &str = concat!("AKIAQN", "PYDW3K2LMNOPQR");
        const NEIGHBOUR: &str = "JO0DBFC29JNAi30cP3MbOjtYnUkcHfQx";
        let det = detect::DETECTORS
            .iter()
            .find(|d| d.id == "aws-access-key-id")
            .unwrap();
        let guard = Engine::new(&detect::resolve(&[], None, &[]).unwrap()).unwrap();

        // Same line and separate line; and with the neighbour pushed far enough
        // out that a naive implementation would clip it mid-value.
        for pad in [0usize, 30, 300] {
            let raw = format!(
                "AWS_ACCESS_KEY_ID={ID}{}\nAWS_SECRET_ACCESS_KEY={NEIGHBOUR}\n",
                " ".repeat(pad)
            );
            let site = extract_site(&raw, ID, Path::new("t.txt"), "test").unwrap();
            let rendered = render_site(&site, det, ID, "deadbeef", &guard);

            for side in [&rendered.before, &rendered.after] {
                assert!(!side.contains(ID), "pad {pad}: target leaked: {side}");
                // No prefix of the neighbour, however short, may survive.
                for n in (8..=NEIGHBOUR.len()).rev() {
                    assert!(
                        !side.contains(&NEIGHBOUR[..n]),
                        "pad {pad}: {n} chars of the neighbouring secret leaked: {side}"
                    );
                }
            }
            assert!(rendered.after.contains("[REDACTED:deadbeef]"));
        }
    }

    #[test]
    fn preview_of_a_jsonl_line_does_not_leak_a_neighbouring_secret() {
        // The case the plain-text test missed: inside a .jsonl line a newline
        // is the two bytes `\` `n`, so scanning raw text mis-bounds the
        // neighbouring value and the guard pass fails to mask it.
        const ID: &str = concat!("AKIAQN", "PYDW3K2LMNOPQR");
        const NEIGHBOUR: &str = "JO0DBFC29JNAi30cP3MbOjtYnUkcHfQx";

        let dir = scratch("preview-jsonl");
        let path = dir.join("s.jsonl");
        let stdout = format!("AWS_ACCESS_KEY_ID={ID}\nAWS_SECRET_ACCESS_KEY={NEIGHBOUR}\n");
        let line = serde_json::json!({ "toolUseResult": { "stdout": stdout } }).to_string();
        assert!(line.contains("\\n"), "fixture must carry escaped newlines");
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let file = ScrubFile {
            path: path.clone(),
            kind: TargetKind::Jsonl,
            label: "test",
            mtime: meta.modified().unwrap(),
            size: meta.len(),
        };

        let engine = Engine::new(&detect::resolve(&[], None, &[]).unwrap()).unwrap();
        let scan = scan_file(&file, &engine, true);
        let site = scan.sites.get(ID).expect("a site for the key id");

        let det = detect::DETECTORS
            .iter()
            .find(|d| d.id == "aws-access-key-id")
            .unwrap();
        let rendered = render_site(site, det, ID, "deadbeef", &engine);

        for side in [&rendered.before, &rendered.after] {
            assert!(!side.contains(ID), "target leaked: {side}");
            for n in (8..=NEIGHBOUR.len()).rev() {
                assert!(
                    !side.contains(&NEIGHBOUR[..n]),
                    "{n} chars of the neighbouring secret leaked: {side}"
                );
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_site_is_located_where_the_value_stands_alone() {
        // A short value can occur inside an unrelated longer token; reporting
        // that position would point the preview at the wrong line entirely.
        let embedded = "TOKEN=Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk";
        let real = "url is zorp://admin:Ff6Gg7Hh8Ii9Jj0@db.internal/x";
        let data = format!("{embedded}\n{real}\n");

        let site = extract_site(&data, "Ff6Gg7Hh8Ii9Jj0", Path::new("t.txt"), "test").unwrap();
        assert_eq!(
            site.line, 2,
            "site should be on the line it stands alone on"
        );
        assert!(site.context.contains("zorp://"));
    }

    #[test]
    fn the_export_pairs_each_secret_with_its_own_replacement() {
        // The pairing must not depend on `findings` ordering: sorting them for
        // display would otherwise hand every entry the wrong plaintext, and the
        // resulting undo map restores secrets into the wrong places.
        const A: &str = "sk-or-v1-cdb75f9a2e1b4c8d6f0a3e5b7c9d1f2a4b6c8d0e2f4a6b8c1d3e5f7a9b0c2d4";
        const B: &str = concat!("AKIAQN", "PYDW3K2LMNOPQR");

        let dir = scratch("export");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();
        let path = store.join("s.jsonl");
        std::fs::write(&path, format!("{{\"a\":\"{A}\",\"b\":\"{B}\"}}\n")).unwrap();

        let targets = vec![ScrubTarget::jsonl(store.clone(), "test")];
        let dets = detect::resolve(&[], None, &[]).unwrap();
        let engine = Engine::new(&dets).unwrap();
        let d = discover(&targets, Duration::from_secs(0), None);
        let scan = scan_file(&d.files[0], &engine, false);

        let mut entries_owned = Vec::new();
        for (value, (det, count)) in &scan.values {
            entries_owned.push((
                value.clone(),
                Finding {
                    detector_id: det.id,
                    category: det.category,
                    fp: "ffffffff".to_string(),
                    full_fp: "f".repeat(64),
                    masked: redact::mask(det, value, "ffffffff"),
                    occurrences: *count,
                    files: 1,
                    wide: false,
                    sites: Vec::new(),
                },
            ));
        }
        let entries: Vec<(String, &Finding)> =
            entries_owned.iter().map(|(v, f)| (v.clone(), f)).collect();

        let out = dir.join("secrets.json");
        write_export(&out, &entries, false).unwrap();

        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        for e in doc["secrets"].as_array().unwrap() {
            let value = e["value"].as_str().unwrap();
            let replacement = e["replacement"].as_str().unwrap();
            let det = detect::DETECTORS
                .iter()
                .find(|d| d.id == e["detector"].as_str().unwrap())
                .unwrap();
            assert_eq!(
                replacement,
                redact::mask(det, value, "ffffffff"),
                "replacement does not correspond to its own value"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "export must not be world-readable");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_export_refuses_to_land_inside_a_scanned_store() {
        let dir = scratch("export-guard");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();
        let targets = vec![ScrubTarget::jsonl(store.clone(), "test")];

        assert!(!export_path_is_safe(&store.join("secrets.json"), &targets));
        assert!(export_path_is_safe(&dir.join("secrets.json"), &targets));

        // A bare relative filename resolves against the working directory.
        // Without that, `parent()` is `""`, canonicalisation fails, and the
        // guard passes everything — including the form the docs recommend.
        let bare = Path::new("secrets.json");
        assert!(
            !export_path_is_safe_from(bare, &store, &targets),
            "relative path inside a scanned store must be refused"
        );
        assert!(export_path_is_safe_from(bare, &dir, &targets));
        assert!(!export_path_is_safe_from(
            Path::new("./secrets.json"),
            &store,
            &targets
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_url_password_that_is_a_common_word_does_not_rewrite_prose() {
        // Regression, found only at full corpus scale. `postgres://x:portal@…`
        // is a genuine URL password, but sweeping the value replaced the word
        // "portal" in 169 files of ordinary prose. The old guard counted files
        // the value was *detected* in (4, under the threshold) rather than
        // files it *appears* in (169).
        let dir = scratch("wideword");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();

        for i in 0..4 {
            std::fs::write(
                store.join(format!("u{i}.jsonl")),
                "{\"a\":\"postgres://svc:portal@localhost/db\"}\n",
            )
            .unwrap();
        }
        for i in 0..20 {
            std::fs::write(
                store.join(format!("w{i}.jsonl")),
                "{\"b\":\"the portal is reachable, see portal_reachable\"}\n",
            )
            .unwrap();
        }

        let targets = vec![ScrubTarget::jsonl(store.clone(), "test")];
        let d = discover(&targets, Duration::from_secs(0), None);
        let dets = detect::resolve(&[], None, &[]).unwrap();
        let engine = Engine::new(&dets).unwrap();

        // First line of defence: a bare lowercase word is not a password, so it
        // is never detected and never reaches the sweep at all.
        let detected = d
            .files
            .iter()
            .filter(|f| scan_file(f, &engine, false).values.contains_key("portal"))
            .count();
        assert_eq!(detected, 0, "the shape gate must reject a dictionary word");

        assert!(!detect::passes_password_shape("portal"));
        assert!(!detect::passes_password_shape("forseti"));
        assert!(!detect::passes_password_shape("stackpit"));
        assert!(!detect::passes_password_shape("pass"));
        assert!(detect::passes_password_shape("hunter2pass"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_spread_guard_counts_files_the_value_appears_in() {
        // Second line of defence, for a value that does pass the shape gate but
        // is still common in prose. Detection spread and corpus spread differ,
        // and the guard must use the larger one.
        let dir = scratch("spread");
        let store = dir.join("store");
        std::fs::create_dir_all(&store).unwrap();

        for i in 0..4 {
            std::fs::write(
                store.join(format!("u{i}.jsonl")),
                "{\"a\":\"postgres://svc:Hunter2Pass@localhost/db\"}\n",
            )
            .unwrap();
        }
        for i in 0..20 {
            std::fs::write(
                store.join(format!("w{i}.jsonl")),
                "{\"b\":\"the Hunter2Pass build step\"}\n",
            )
            .unwrap();
        }

        let targets = vec![ScrubTarget::jsonl(store.clone(), "test")];
        let d = discover(&targets, Duration::from_secs(0), None);
        let dets = detect::resolve(&[], None, &[]).unwrap();
        let engine = Engine::new(&dets).unwrap();

        let detected = d
            .files
            .iter()
            .filter(|f| {
                scan_file(f, &engine, false)
                    .values
                    .contains_key("Hunter2Pass")
            })
            .count();
        let spread = count_value_spread(&d.files, &["Hunter2Pass".to_string()], None);

        assert_eq!(detected, 4, "detected only where the URL is");
        assert_eq!(spread["Hunter2Pass"], 24, "but present in every file");
        assert!(
            spread["Hunter2Pass"] > 5,
            "corpus spread must exceed --max-spread so the sweep is held back"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_with_no_secrets_is_byte_identical_after_apply() {
        let dir = scratch("negative");
        let path = dir.join("clean.jsonl");
        let body = "{\"z\":1,\"a\":\"nothing here\"}\n{\"b\":[1,2,3]}\n";
        std::fs::write(&path, body).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        let file = ScrubFile {
            path: path.clone(),
            kind: TargetKind::Jsonl,
            label: "test",
            mtime: meta.modified().unwrap(),
            size: meta.len(),
        };

        let det = detect::DETECTORS
            .iter()
            .find(|d| d.id == "openrouter-key")
            .unwrap();
        let matcher = Matcher::new(&[Secret {
            value: "sk-or-v1-cdb75f9a2e1b4c8d6f0a3e5b7c9d1f2a4b6c8d0e2f4a6b8c".to_string(),
            detector: det,
            fp: "a3f91c04".to_string(),
        }])
        .unwrap();

        assert!(!rewrite_file(&file, &matcher, Duration::from_secs(0)).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn end_to_end_scan_then_apply_then_rescan_is_clean() {
        const KEY: &str = "sk-or-v1-cdb75f9a2e1b4c8d6f0a3e5b7c9d1f2a4b6c8d0e2f4a6b8c";
        let dir = scratch("e2e");
        let path = dir.join("session.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"toolUseResult\":{{\"stdout\":\"OPENROUTER_API_KEY={KEY}\"}}}}\n\
                 {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"input\":{{\"command\":\"curl -H 'Bearer {KEY}'\"}}}}]}}}}\n"
            ),
        )
        .unwrap();

        let targets = vec![ScrubTarget::jsonl(dir.clone(), "test")];
        let dets = detect::resolve(&[], None, &[]).unwrap();
        let engine = Engine::new(&dets).unwrap();

        let d = discover(&targets, Duration::from_secs(0), None);
        assert_eq!(d.files.len(), 1);
        let scan = scan_file(&d.files[0], &engine, false);
        assert!(scan.values.keys().any(|v| v == KEY));

        let det = detect::DETECTORS
            .iter()
            .find(|x| x.id == "openrouter-key")
            .unwrap();
        let matcher = Matcher::new(&[Secret {
            value: KEY.to_string(),
            detector: det,
            fp: "a3f91c04".to_string(),
        }])
        .unwrap();
        assert!(rewrite_file(&d.files[0], &matcher, Duration::from_secs(0)).unwrap());

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains(KEY));
        assert_eq!(after.lines().count(), 2);
        for line in after.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }

        let d2 = discover(&targets, Duration::from_secs(0), None);
        assert!(scan_file(&d2.files[0], &engine, false).values.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
