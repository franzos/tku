//! Multi-account support for Claude Code.
//!
//! tku stashes credentials (`.credentials.json`) under
//! `~/.config/tku/accounts/claude/<name>.credentials.json` and records every
//! swap in a registry file, so historical usage records can be attributed to
//! the account that was active at the time.
//!
//! Design notes:
//! - Only `.credentials.json` is swapped. Everything else in `~/.claude/`
//!   (skills, CLAUDE.md, settings, hooks) stays shared.
//! - Account key is `organizationUuid` from the creds file. Names are aliases.
//! - On every tku run we detect credential changes that happened outside
//!   tku and append an "implicit" switch entry with a soft warning.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::paths;
use crate::types::UsageRecord;

const TOOL_CLAUDE: &str = "claude";

// --- Types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub org_uuid: String,
    pub added_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    #[serde(default)]
    pub subscription_type: Option<String>,
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchEntry {
    pub at: DateTime<Utc>,
    pub org_uuid: String,
    pub name: String,
    pub source: SwitchSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchSource {
    /// User ran `tku account use`
    Explicit,
    /// Credential change detected outside tku on startup
    Implicit,
    /// First-run auto-registration
    Bootstrap,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub switch_log: Vec<SwitchEntry>,
}

fn default_version() -> u32 {
    1
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: 1,
            accounts: Vec::new(),
            switch_log: Vec::new(),
        }
    }
}

impl Registry {
    pub fn find_by_name(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.name == name)
    }

    pub fn find_by_org(&self, org_uuid: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.org_uuid == org_uuid)
    }

    pub fn latest_switch(&self) -> Option<&SwitchEntry> {
        self.switch_log.iter().max_by_key(|e| e.at)
    }

    /// Account active at a given timestamp: the most recent switch-log entry
    /// whose `at` is ≤ ts.
    pub fn account_at(&self, ts: DateTime<Utc>) -> Option<&SwitchEntry> {
        self.switch_log
            .iter()
            .filter(|e| e.at <= ts)
            .max_by_key(|e| e.at)
    }
}

// --- Paths ---

fn registry_path(tool: &str) -> Option<PathBuf> {
    paths::registry_file(tool)
}

pub fn stashed_creds_path(tool: &str, name: &str) -> Option<PathBuf> {
    paths::accounts_dir(tool).map(|d| d.join(format!("{name}.credentials.json")))
}

pub fn claude_creds_path() -> Option<PathBuf> {
    BaseDirs::new().map(|b| b.home_dir().join(".claude").join(".credentials.json"))
}

/// Replace the user's home-dir prefix with `~` for user-visible paths.
/// Leaves paths outside `$HOME` untouched. Avoids leaking the username in
/// error messages that may be shared in bug reports.
pub(crate) fn redact(path: &Path) -> String {
    if let Some(base) = BaseDirs::new() {
        let home = base.home_dir();
        if let Ok(rest) = path.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Create the credential stash parent dir with `0o700` on Unix so siblings
/// can't enumerate account names. Falls back to plain `create_dir_all` on
/// non-Unix targets where mode-at-creation isn't expressible.
fn create_stash_parent(parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(parent)
        {
            Ok(()) => Ok(()),
            // DirBuilder with recursive=true succeeds even if the dir already
            // exists — we don't need to special-case that here. Propagate
            // other errors unchanged.
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(parent)
    }
}

// --- Registry I/O ---

pub fn load_registry(tool: &str) -> Registry {
    let Some(path) = registry_path(tool) else {
        return Registry::default();
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return Registry::default();
    };
    match serde_json::from_str(&data) {
        Ok(reg) => reg,
        Err(e) => {
            // Don't silently drop history — back up the corrupt file so the
            // user can recover the switch log manually.
            let backup = path.with_extension("json.bak");
            if let Err(be) = fs::copy(&path, &backup) {
                eprintln!(
                    "⚠ Account registry at {} is corrupt ({}). Failed to back up: {}. Starting fresh.",
                    redact(&path),
                    e,
                    be
                );
            } else {
                eprintln!(
                    "⚠ Account registry at {} is corrupt ({}). Backed up to {} and starting fresh.",
                    redact(&path),
                    e,
                    redact(&backup)
                );
            }
            Registry::default()
        }
    }
}

fn save_registry(tool: &str, registry: &Registry) -> Result<()> {
    let path = registry_path(tool).context("cannot determine registry path")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", redact(parent)))?;
    }
    let data = serde_json::to_string_pretty(registry).context("serialize registry")?;
    // Atomic write: two concurrent `tku` invocations switching accounts must
    // not be able to truncate each other's switch-log entries. The registry
    // also carries the canonical org-UUID → name mapping, so a torn write
    // here would corrupt attribution for every subsequent run.
    atomic_write(&path, data.as_bytes(), Some(0o600))
        .with_context(|| format!("write {}", redact(&path)))?;
    Ok(())
}

// --- Credential inspection ---

#[derive(Debug, Clone)]
struct CredsInfo {
    /// `organizationUuid` when present. Claude Code drops this field after an
    /// access-token refresh, so we treat it as optional.
    org_uuid: Option<String>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

/// Read `organizationUuid` from the live Claude credentials file.
/// Returns None when the file is missing, unparseable, or has been stripped
/// of the field by a recent token refresh — providers should fall back to
/// the registry's last-known active account in that case.
pub fn current_claude_org_uuid() -> Option<String> {
    read_current_claude_creds_info().and_then(|i| i.org_uuid)
}

fn read_current_claude_creds_info() -> Option<CredsInfo> {
    let path = claude_creds_path()?;
    let data = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    // A credentials file without the OAuth object is unusable for any purpose.
    value.get("claudeAiOauth")?;
    let org_uuid = value
        .get("organizationUuid")
        .and_then(|v| v.as_str())
        .map(String::from);
    let subscription_type = value
        .pointer("/claudeAiOauth/subscriptionType")
        .and_then(|v| v.as_str())
        .map(String::from);
    let rate_limit_tier = value
        .pointer("/claudeAiOauth/rateLimitTier")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(CredsInfo {
        org_uuid,
        subscription_type,
        rate_limit_tier,
    })
}

/// Write `data` to `path` atomically with mode 0600 from the moment of
/// creation (no post-hoc chmod window).
fn write_secure(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    atomic_write(path, data, Some(0o600))
}

fn stash_creds_file(tool: &str, name: &str, src: &std::path::Path) -> Result<PathBuf> {
    let dst = stashed_creds_path(tool, name).context("cannot determine stash path")?;
    if let Some(parent) = dst.parent() {
        create_stash_parent(parent).with_context(|| format!("create {}", redact(parent)))?;
    }
    let data = fs::read(src).with_context(|| format!("read {}", redact(src)))?;
    write_secure(&dst, &data).with_context(|| format!("write {}", redact(&dst)))?;
    Ok(dst)
}

// --- Bootstrap / implicit-swap detection ---

/// Pre-scan hook: append an implicit-switch entry if the live credentials'
/// `organizationUuid` no longer matches the most recent switch-log entry.
///
/// This must run **before** the provider scan so per-record attribution
/// (which looks up `account_at(timestamp)` for each record) sees a switch
/// log that already reflects the external swap. Without it, records written
/// under the new account would still be looked up against an out-of-date
/// log and get tagged with the previous account.
///
/// Skips silently when the registry is empty (deferred to post-scan
/// bootstrap, which can anchor the bootstrap entry to the earliest record
/// timestamp instead of to `Utc::now()`).
pub fn detect_implicit_swap_pre_scan() {
    let Some(info) = read_current_claude_creds_info() else {
        return;
    };
    // No org UUID in creds (Claude Code dropped it after a refresh) means
    // we can't tell whether a swap happened — bail without warning.
    let Some(current_org) = info.org_uuid else {
        return;
    };
    let mut registry = load_registry(TOOL_CLAUDE);
    if registry.accounts.is_empty() {
        return;
    }
    let Some(last) = registry.latest_switch().cloned() else {
        return;
    };
    if last.org_uuid == current_org {
        return;
    }

    let known = registry.find_by_org(&current_org).cloned();
    let name = match &known {
        Some(a) => a.name.clone(),
        None => format!("unknown-{}", short_org(&current_org)),
    };

    eprintln!("⚠ Credential change detected outside tku.");
    eprintln!(
        "  Previous: {} (org: {})",
        last.name,
        short_org(&last.org_uuid)
    );
    eprintln!("  Current:  {} (org: {})", name, short_org(&current_org));
    eprintln!(
        "  Records between runs are attributed to '{}'. Use `tku account use",
        last.name
    );
    eprintln!("  <name>` next time for precise attribution.");
    eprintln!();

    registry.switch_log.push(SwitchEntry {
        at: Utc::now(),
        org_uuid: current_org,
        name,
        source: SwitchSource::Implicit,
    });
    if let Err(e) = save_registry(TOOL_CLAUDE, &registry) {
        eprintln!("warning: failed to persist implicit-swap entry: {e}");
    }
}

/// Post-scan bootstrap: register the current credentials as "default" the
/// first time we see them. Anchors the bootstrap switch entry to the
/// earliest record timestamp so historical records get attributed correctly
/// instead of all collapsing to "Utc::now()".
pub fn bootstrap_if_needed_post_scan(claude_records: &[&UsageRecord]) -> Option<String> {
    let info = read_current_claude_creds_info()?;
    let current_org = info.org_uuid?;
    let mut registry = load_registry(TOOL_CLAUDE);
    if !registry.accounts.is_empty() {
        return registry.latest_switch().map(|s| s.name.clone());
    }

    let earliest = claude_records
        .iter()
        .map(|r| r.timestamp)
        .min()
        .unwrap_or_else(Utc::now);
    let now = Utc::now();

    registry.accounts.push(Account {
        name: "default".to_string(),
        org_uuid: current_org.clone(),
        added_at: now,
        last_used_at: now,
        subscription_type: info.subscription_type,
        rate_limit_tier: info.rate_limit_tier,
    });
    registry.switch_log.push(SwitchEntry {
        at: earliest,
        org_uuid: current_org,
        name: "default".to_string(),
        source: SwitchSource::Bootstrap,
    });

    if let Some(creds) = claude_creds_path() {
        if let Err(e) = stash_creds_file(TOOL_CLAUDE, "default", &creds) {
            eprintln!("warning: failed to stash credentials for 'default': {e}");
        }
    }

    if let Err(e) = save_registry(TOOL_CLAUDE, &registry) {
        eprintln!("warning: failed to persist account registry: {e}");
    }
    Some("default".to_string())
}

fn short_org(org: &str) -> String {
    org.chars().take(8).collect()
}

// --- Account commands ---

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Account name cannot be empty");
    }
    if name.len() > 64 {
        bail!("Account name is too long (max 64 chars)");
    }
    if name.starts_with('-') {
        bail!("Account name cannot start with '-' (looks like a flag)");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!(
            "Account name must match [A-Za-z0-9_-] (no spaces, paths, control chars, or Unicode)"
        );
    }
    Ok(())
}

pub fn add(name: &str) -> Result<()> {
    validate_name(name)?;
    let src = claude_creds_path().context("cannot find credentials path")?;

    // Skip TOCTOU `exists()` check — let `fs::read` surface a missing-file
    // error with context. Avoids a race where the file disappears between
    // the existence check and the read.
    let data = fs::read(&src).with_context(|| {
        format!(
            "Cannot read credentials at {}. Run Claude Code at least once first.",
            redact(&src)
        )
    })?;

    let info = read_current_claude_creds_info()
        .ok_or_else(|| anyhow!("Credentials file is missing or unparseable"))?;
    let org_uuid = info.org_uuid.ok_or_else(|| {
        anyhow!(
            "Credentials file has no organizationUuid (Claude Code may have just refreshed the token).\n\
             Start Claude Code once to regenerate the field, then try again."
        )
    })?;
    let sub_type = info.subscription_type;
    let rate_tier = info.rate_limit_tier;

    let mut registry = load_registry(TOOL_CLAUDE);
    if registry.find_by_name(name).is_some() {
        bail!(
            "Account '{name}' already exists. Use `tku account rename` to rename or `tku account remove` to delete."
        );
    }
    if let Some(existing) = registry.find_by_org(&org_uuid) {
        bail!(
            "This Claude login (org {}) is already saved as '{}'.\n\
             To save a different account, log out of Claude Code and log back in with the other one first:\n  \
               claude /logout\n  \
               claude /login\n  \
               tku account add {}",
            short_org(&org_uuid),
            existing.name,
            name
        );
    }

    // Stash a copy of the bytes we already loaded (avoid re-reading the src).
    let dst = stashed_creds_path(TOOL_CLAUDE, name).context("cannot determine stash path")?;
    if let Some(parent) = dst.parent() {
        create_stash_parent(parent).with_context(|| format!("create {}", redact(parent)))?;
    }
    write_secure(&dst, &data).with_context(|| format!("write {}", redact(&dst)))?;

    let now = Utc::now();
    registry.accounts.push(Account {
        name: name.to_string(),
        org_uuid: org_uuid.clone(),
        added_at: now,
        last_used_at: now,
        subscription_type: sub_type,
        rate_limit_tier: rate_tier,
    });
    save_registry(TOOL_CLAUDE, &registry)?;

    eprintln!(
        "Registered account '{}' (org: {}).",
        name,
        short_org(&org_uuid)
    );
    Ok(())
}

pub fn use_account(name: &str, force: bool) -> Result<()> {
    validate_name(name)?;
    let mut registry = load_registry(TOOL_CLAUDE);
    let account = registry.find_by_name(name).cloned().ok_or_else(|| {
        anyhow!("Account '{name}' not found. Run `tku account list` to see available accounts.")
    })?;

    // Refuse to clobber a live login that hasn't been saved — switching would
    // delete the only copy of those credentials. The implicit-swap detector
    // catches this *after* the fact; this check stops it from happening.
    if !force {
        if let Some(info) = read_current_claude_creds_info() {
            if let Some(current_org) = info.org_uuid {
                if current_org != account.org_uuid && registry.find_by_org(&current_org).is_none() {
                    bail!(
                        "Current Claude login (org {}) isn't saved — switching would lose it.\n\
                         Save it first:\n  \
                           tku account add <name>\n\
                         Or pass --force to overwrite anyway.",
                        short_org(&current_org)
                    );
                }
            }
        }
    }

    let src = stashed_creds_path(TOOL_CLAUDE, name).context("cannot determine stash path")?;
    if !src.exists() {
        bail!(
            "Stashed credentials missing: {}. Registry is out of sync.",
            redact(&src)
        );
    }
    let dst = claude_creds_path().context("cannot find credentials path")?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", redact(parent)))?;
    }
    let data = fs::read(&src).with_context(|| format!("read {}", redact(&src)))?;
    write_secure(&dst, &data).with_context(|| format!("write {}", redact(&dst)))?;

    let now = Utc::now();
    registry.switch_log.push(SwitchEntry {
        at: now,
        org_uuid: account.org_uuid.clone(),
        name: name.to_string(),
        source: SwitchSource::Explicit,
    });
    if let Some(a) = registry.accounts.iter_mut().find(|a| a.name == name) {
        a.last_used_at = now;
    }
    save_registry(TOOL_CLAUDE, &registry)?;

    eprintln!(
        "Switched to '{}' (org: {}).",
        name,
        short_org(&account.org_uuid)
    );
    eprintln!("Claude Code will refresh the access token on next launch if needed.");
    Ok(())
}

pub fn list() -> Result<()> {
    let registry = load_registry(TOOL_CLAUDE);
    // Prefer identifying the active account from live creds; fall back to the
    // latest switch-log entry when Claude Code has hidden `organizationUuid`
    // after a token refresh.
    let active_org = read_current_claude_creds_info()
        .and_then(|i| i.org_uuid)
        .or_else(|| registry.latest_switch().map(|s| s.org_uuid.clone()));

    if registry.accounts.is_empty() {
        eprintln!("No accounts registered.");
        eprintln!();
        eprintln!("Run `tku sub` once to auto-register your current account as 'default',");
        eprintln!("or `tku account add <name>` to register under a custom name.");
        return Ok(());
    }

    println!("Accounts (claude):");
    for a in &registry.accounts {
        let is_active = active_org.as_deref() == Some(a.org_uuid.as_str());
        let marker = if is_active { "*" } else { " " };
        let plan = format_plan(a.subscription_type.as_deref(), a.rate_limit_tier.as_deref());
        println!(
            "  {} {:<20} org: {}  {}",
            marker,
            a.name,
            short_org(&a.org_uuid),
            plan
        );
    }
    println!();
    println!("* = currently active");
    Ok(())
}

pub fn current() -> Result<()> {
    let registry = load_registry(TOOL_CLAUDE);
    let info = read_current_claude_creds_info();

    match info {
        Some(info) => {
            let plan = format_plan(
                info.subscription_type.as_deref(),
                info.rate_limit_tier.as_deref(),
            );
            match info.org_uuid {
                Some(org) => {
                    let name = registry
                        .find_by_org(&org)
                        .map(|a| a.name.as_str())
                        .unwrap_or("<unregistered>");
                    println!("Active: {} (org: {}, {})", name, short_org(&org), plan);
                }
                None => {
                    // Fall back to registry for a display name; the creds file
                    // is valid but doesn't carry organizationUuid.
                    let name = registry
                        .latest_switch()
                        .map(|s| s.name.as_str())
                        .unwrap_or("<unknown>");
                    println!("Active: {name} ({plan}, org hidden by Claude Code)");
                }
            }
        }
        None => {
            eprintln!("No active Claude credentials found.");
        }
    }
    Ok(())
}

pub fn rename(old: &str, new: &str) -> Result<()> {
    validate_name(old)?;
    validate_name(new)?;
    if old == new {
        bail!("Old and new names are identical");
    }

    let mut registry = load_registry(TOOL_CLAUDE);
    if registry.find_by_name(new).is_some() {
        bail!("Account '{new}' already exists.");
    }
    if registry.find_by_name(old).is_none() {
        bail!("Account '{old}' not found.");
    }

    let old_path = stashed_creds_path(TOOL_CLAUDE, old).context("stash path")?;
    let new_path = stashed_creds_path(TOOL_CLAUDE, new).context("stash path")?;
    if old_path.exists() {
        fs::rename(&old_path, &new_path)
            .with_context(|| format!("rename {} → {}", redact(&old_path), redact(&new_path)))?;
    }

    for a in registry.accounts.iter_mut() {
        if a.name == old {
            a.name = new.to_string();
        }
    }
    for e in registry.switch_log.iter_mut() {
        if e.name == old {
            e.name = new.to_string();
        }
    }
    save_registry(TOOL_CLAUDE, &registry)?;

    eprintln!("Renamed '{old}' → '{new}'.");
    Ok(())
}

pub fn remove(name: &str, force: bool) -> Result<()> {
    validate_name(name)?;
    let mut registry = load_registry(TOOL_CLAUDE);
    let idx = registry
        .accounts
        .iter()
        .position(|a| a.name == name)
        .ok_or_else(|| anyhow!("Account '{name}' not found."))?;

    // Removing the live account would delete the only saved copy *and* leave
    // ~/.claude/.credentials.json in an unsaved state — easy to do by accident.
    if !force {
        let target_org = registry.accounts[idx].org_uuid.clone();
        let active_org = read_current_claude_creds_info().and_then(|i| i.org_uuid);
        if active_org.as_deref() == Some(target_org.as_str()) {
            bail!(
                "'{name}' is the currently-active account. Switch away first with `tku account use <other>`,\n\
                 or pass --force to remove it anyway (the live credentials file will stay in place but\n\
                 won't be saved anywhere)."
            );
        }
    }

    registry.accounts.remove(idx);

    if let Some(path) = stashed_creds_path(TOOL_CLAUDE, name) {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("remove {}", redact(&path)))?;
        }
    }

    save_registry(TOOL_CLAUDE, &registry)?;

    eprintln!("Removed account '{name}'.");
    eprintln!("Switch log entries preserved for historical attribution.");
    Ok(())
}

fn format_plan(sub_type: Option<&str>, rate_tier: Option<&str>) -> String {
    let plan = match sub_type {
        Some("max") => "Claude Max",
        Some("pro") => "Claude Pro",
        Some(other) => other,
        None => "unknown",
    };
    let tier = rate_tier.unwrap_or("");
    let multiplier = if tier.contains("20x") {
        " (20x)"
    } else if tier.contains("5x") {
        " (5x)"
    } else {
        ""
    };
    format!("{plan}{multiplier}")
}
