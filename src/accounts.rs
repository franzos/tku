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
    /// Cached `oauthAccount` blob in the shape Claude Code writes to
    /// `~/.claude.json`. Populated from the profile API at `add`
    /// time and re-applied on `use_account` so Claude Code's UI reflects
    /// the swapped identity (Claude Code only writes this file at first
    /// onboarding, never on subsequent /login).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_account: Option<serde_json::Value>,
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

#[derive(Clone)]
struct CredsInfo {
    /// `organizationUuid` from `.credentials.json` when the field is present
    /// in that file (legacy layout). Today's Claude Code doesn't write it
    /// here — `add()` resolves the field via the profile API instead.
    /// Note: `~/.claude.json:oauthAccount.organizationUuid` exists
    /// but is only written at first onboarding and isn't refreshed on
    /// subsequent logout/login, so we deliberately don't read from it.
    org_uuid: Option<String>,
    access_token: Option<String>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

// Hand-written Debug that omits the access token: any `{:?}` print of a
// CredsInfo (or a struct that contains one) would otherwise leak the bearer
// token into logs or error chains.
impl std::fmt::Debug for CredsInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredsInfo")
            .field("org_uuid", &self.org_uuid)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("subscription_type", &self.subscription_type)
            .field("rate_limit_tier", &self.rate_limit_tier)
            .finish()
    }
}

/// Read the live account's org UUID from the creds file. Returns None on
/// modern Claude Code layouts that don't store the field there — callers
/// that need a guaranteed UUID must hit the profile API.
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
    let access_token = value
        .pointer("/claudeAiOauth/accessToken")
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
        access_token,
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
    // Capture the freshly-rotated tokens for the new active account too,
    // not just the switch fact — otherwise the next swap-back from another
    // account would restore a stale vault entry.
    if let Err(e) = snapshot_live_into_active_vault(&mut registry) {
        eprintln!("warning: could not snapshot live credentials: {e}");
    }
    if let Err(e) = save_registry(TOOL_CLAUDE, &registry) {
        eprintln!("warning: failed to persist implicit-swap entry: {e}");
    }
}

/// Cheap reconciliation pass: keep the active account's vault and plan
/// metadata current with whatever Claude Code wrote to
/// `~/.claude/.credentials.json` since our last run. This covers the gap
/// where the user runs `claude /login` (or Claude refreshes the token in
/// the background) without subsequently invoking `tku account use` — the
/// fresh tokens would otherwise sit only in the live file and be lost on
/// the next swap-back.
///
/// Designed to be called on every `tku` invocation except for `account use`
/// (which runs its own snapshot inline) and `account` subcommands that
/// don't touch credentials. No-op when the live token hasn't changed.
pub fn reconcile_live_creds() {
    let mut registry = load_registry(TOOL_CLAUDE);
    if registry.accounts.is_empty() {
        return;
    }
    // Cheap check up-front: if the live token already matches the active
    // vault, skip both the identity verification and the registry write.
    let Some(active) = registry.latest_switch().cloned() else {
        return;
    };
    let Some(idx) = registry
        .accounts
        .iter()
        .position(|a| a.org_uuid == active.org_uuid)
    else {
        return;
    };
    let live_token = read_current_claude_creds_info().and_then(|i| i.access_token);
    let vault_token = stashed_creds_path(TOOL_CLAUDE, &registry.accounts[idx].name)
        .and_then(|p| fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .as_ref()
        .and_then(|v| {
            v.pointer("/claudeAiOauth/accessToken")
                .and_then(|t| t.as_str())
                .map(String::from)
        });
    if live_token.is_none() || live_token == vault_token {
        return;
    }
    if let Err(e) = snapshot_live_into_active_vault(&mut registry) {
        eprintln!("warning: could not snapshot live credentials: {e}");
        return;
    }
    if let Err(e) = save_registry(TOOL_CLAUDE, &registry) {
        eprintln!("warning: failed to persist registry after snapshot: {e}");
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
        oauth_account: None,
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
    // Modern Claude Code creds files don't carry `organizationUuid`, so we
    // resolve identity via the profile API using the live access token.
    // We also stash the resulting `oauthAccount` blob so `use_account` can
    // keep `.claude.json` in sync with the swapped credentials — Claude
    // Code only writes that file at first onboarding, never on /login.
    let token = info.access_token.ok_or_else(|| {
        anyhow!(
            "Credentials file has no access token. Sign in to Claude Code first, then try again."
        )
    })?;
    let identity = crate::subscription::fetch_account_identity(&token)
        .context("Could not fetch account identity from the Anthropic profile API")?;
    let org_uuid = identity.org_uuid.clone();
    let oauth_account = Some(identity.oauth_account);
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
        oauth_account,
    });
    // Append a switch-log entry so consumers that infer the active account
    // from `latest_switch()` (list, current, subscription keying) see the
    // newly-added account as live. This is correct by construction — `add`
    // can only register the account that owns the currently-live creds.
    // Tagged Implicit because the actual swap happened outside tku at an
    // unknown earlier time; we anchor to `now` since we can't recover it.
    let needs_switch_entry = registry
        .latest_switch()
        .map(|s| s.org_uuid != org_uuid)
        .unwrap_or(true);
    if needs_switch_entry {
        registry.switch_log.push(SwitchEntry {
            at: now,
            org_uuid: org_uuid.clone(),
            name: name.to_string(),
            source: SwitchSource::Implicit,
        });
    }
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

    // Reconcile the registry with reality before we make swap decisions. If
    // the user did `claude /logout && /login` since our last invocation,
    // `latest_switch` is stale and the snapshot step below would write the
    // fresh login's tokens into the *previous* account's vault — exactly the
    // cross-account contamination we're trying to prevent. Running the
    // implicit detector first appends an Implicit switch entry so
    // `latest_switch` reflects the live creds' actual owner.
    detect_implicit_swap_pre_scan();

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

    // Snapshot the live credentials back into the currently-active vault
    // entry. Claude Code rotates tokens on launch, refresh, and `/login`
    // without ever writing them back to our stash; without this step, the
    // next swap-back would clobber the live file with a stale token and
    // potentially an invalidated refresh token. Identity gated below; safe
    // to call before deciding which account to switch to.
    if let Err(e) = snapshot_live_into_active_vault(&mut registry) {
        // Snapshot is best-effort: a failure here means the *next* swap-back
        // may use a stale token, but the current swap is still safe to
        // proceed with. Don't fail the user's `account use` over it.
        eprintln!("warning: could not snapshot live credentials before swap: {e}");
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

    // Pre-swap migration: if `.claude.json:oauthAccount` matches a registered
    // account that hasn't had its identity stored yet, capture it now —
    // before we overwrite the file. This catches the original-onboarding
    // account, whose stashed access token may already be too old to use the
    // profile API as a fallback path.
    if let Some(current_oauth) = read_current_claude_oauth_account() {
        if let Some(current_org_uuid) = current_oauth
            .get("organizationUuid")
            .and_then(|v| v.as_str())
        {
            let owned_uuid = current_org_uuid.to_string();
            if let Some(matching) = registry
                .accounts
                .iter_mut()
                .find(|a| a.org_uuid == owned_uuid && a.oauth_account.is_none())
            {
                matching.oauth_account = Some(current_oauth);
            }
        }
    }

    let data = fs::read(&src).with_context(|| format!("read {}", redact(&src)))?;
    write_secure(&dst, &data).with_context(|| format!("write {}", redact(&dst)))?;

    // Re-read the (possibly migrated) account record before deciding whether
    // to backfill via API.
    let account = registry
        .find_by_name(name)
        .cloned()
        .expect("account existed at top of function");

    // Backfill the `oauthAccount` blob for legacy entries that pre-date the
    // field. After the creds swap above, the live access token belongs to
    // this account, so the profile API will identify it correctly. If the
    // stashed token is expired, this will 401 — we warn and continue.
    // Pass the in-memory bytes we just wrote (avoids a re-read TOCTOU).
    let oauth_account = if account.oauth_account.is_some() {
        account.oauth_account.clone()
    } else {
        match fetch_oauth_account_from_creds_bytes(&data) {
            Ok(blob) => {
                if let Some(a) = registry.accounts.iter_mut().find(|a| a.name == name) {
                    a.oauth_account = Some(blob.clone());
                }
                Some(blob)
            }
            Err(e) => {
                eprintln!(
                    "warning: could not refresh stored identity for '{name}' (Claude Code's UI will display the previous account until you launch claude once and re-run this command): {e}"
                );
                None
            }
        }
    };

    if let Some(blob) = &oauth_account {
        if let Err(e) = apply_oauth_account_to_claude_config(blob) {
            eprintln!("warning: could not update ~/.claude.json: {e}");
        }
    }

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
    eprintln!(
        "Note: any `claude` session already running in another terminal will hit a 401 on its next refresh and need to be re-launched."
    );
    Ok(())
}

/// Read the current `oauthAccount` object from `~/.claude.json`,
/// if present. Used by the pre-swap migration to capture the originally-
/// onboarded account's identity before it gets overwritten.
fn read_current_claude_oauth_account() -> Option<serde_json::Value> {
    let path = BaseDirs::new()?.home_dir().join(".claude.json");
    let data = fs::read_to_string(&path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&data).ok()?;
    value.get("oauthAccount").cloned()
}

/// Parse a credentials JSON blob and ask the profile API for the matching
/// `oauthAccount` shape. Used to backfill legacy registry entries that
/// pre-date the cached `oauth_account` field. Takes raw bytes so callers can
/// reuse data they've already loaded — avoids a re-read TOCTOU window.
fn fetch_oauth_account_from_creds_bytes(data: &[u8]) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(data).context("parse credentials")?;
    let token = value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("credentials file has no access token"))?;
    Ok(crate::subscription::fetch_account_identity(token)?.oauth_account)
}

/// Snapshot the live `~/.claude/.credentials.json` back into the
/// currently-active vault entry, refreshing the registry's plan metadata at
/// the same time. Identity-gated: we only overwrite the vault if the live
/// creds demonstrably belong to the currently-active account.
///
/// Behaviour:
/// - No-op when live creds are absent, unparseable, or have no `accessToken`.
/// - No-op when the vault already holds an identical `accessToken` (the
///   common case — Claude hasn't rotated since our last snapshot).
/// - Identity check: live creds must match the active account's `org_uuid`.
///   We try the live `organizationUuid` field first (legacy creds layout, no
///   network); if absent we fall back to the profile API. On API failure we
///   warn and skip — better to keep a slightly-stale vault than overwrite
///   the wrong one. We deliberately do *not* fall back to comparing
///   `subscriptionType`/`rateLimitTier`: two accounts on the same plan would
///   collide and the failure mode is silent cross-account corruption.
/// - On match: write the previous vault contents to `<name>.previous` for
///   one-step rollback, then atomically overwrite the vault with the live
///   bytes (same bytes we read for the comparison — no re-read window).
/// - Refresh `subscription_type` / `rate_limit_tier` on the active account so
///   `tku account list` displays the live plan instead of whatever was
///   captured at `add` time. The caller persists the registry.
fn snapshot_live_into_active_vault(registry: &mut Registry) -> Result<()> {
    let Some(active_entry) = registry.latest_switch().cloned() else {
        return Ok(());
    };
    let Some(active_idx) = registry
        .accounts
        .iter()
        .position(|a| a.org_uuid == active_entry.org_uuid)
    else {
        // Active entry references an org_uuid we no longer have a vault for
        // (e.g. user ran `account remove --force` on the live account).
        // Nothing to snapshot into.
        return Ok(());
    };
    let active_org = registry.accounts[active_idx].org_uuid.clone();
    let active_name = registry.accounts[active_idx].name.clone();

    let live_path = claude_creds_path().context("cannot find credentials path")?;
    let live_bytes = match fs::read(&live_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow!("read {}: {}", redact(&live_path), e)),
    };
    let live_value: serde_json::Value = match serde_json::from_slice(&live_bytes) {
        Ok(v) => v,
        Err(_) => return Ok(()), // Claude rewriting mid-read; try again next swap.
    };
    let Some(live_token) = live_value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
    else {
        return Ok(());
    };

    let vault_path = stashed_creds_path(TOOL_CLAUDE, &active_name).context("vault path")?;
    let vault_bytes = fs::read(&vault_path).ok();
    let vault_token = vault_bytes
        .as_deref()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
        .as_ref()
        .and_then(|v| {
            v.pointer("/claudeAiOauth/accessToken")
                .and_then(|t| t.as_str())
                .map(String::from)
        });
    if vault_token.as_deref() == Some(live_token) {
        return Ok(()); // Vault is already current.
    }

    // Identity verification: live → active org. Cheap path first.
    let live_org = live_value
        .get("organizationUuid")
        .and_then(|v| v.as_str())
        .map(String::from);
    let identity_ok = match live_org {
        Some(ref org) => org == &active_org,
        None => match crate::subscription::fetch_account_identity(live_token) {
            Ok(id) => id.org_uuid == active_org,
            Err(e) => {
                eprintln!(
                    "warning: live credentials carry no organizationUuid and the profile API \
                     could not confirm their owner ({e}). Skipping vault snapshot for \
                     '{active_name}' — the next swap may restore a stale token."
                );
                return Ok(());
            }
        },
    };
    if !identity_ok {
        eprintln!(
            "warning: live credentials don't belong to the currently-active account \
             '{active_name}'. Skipping vault snapshot to avoid cross-account contamination."
        );
        return Ok(());
    }

    // Identity confirmed: write a rollback backup, then overwrite the vault
    // with the exact bytes we just verified.
    if let Some(prev) = &vault_bytes {
        let backup = vault_path.with_file_name(format!("{active_name}.previous.credentials.json"));
        if let Err(e) = write_secure(&backup, prev) {
            eprintln!(
                "warning: could not write rollback backup {}: {e}",
                redact(&backup)
            );
        }
    }
    write_secure(&vault_path, &live_bytes)
        .with_context(|| format!("snapshot live creds → {}", redact(&vault_path)))?;

    // Refresh plan metadata so `tku account list` shows the live plan. These
    // are advisory display fields — fall back to existing values if missing.
    let new_sub = live_value
        .pointer("/claudeAiOauth/subscriptionType")
        .and_then(|v| v.as_str())
        .map(String::from);
    let new_tier = live_value
        .pointer("/claudeAiOauth/rateLimitTier")
        .and_then(|v| v.as_str())
        .map(String::from);
    let acct = &mut registry.accounts[active_idx];
    if new_sub.is_some() {
        acct.subscription_type = new_sub;
    }
    if new_tier.is_some() {
        acct.rate_limit_tier = new_tier;
    }
    Ok(())
}

/// Patch `~/.claude.json` so its `oauthAccount` key matches `blob`.
/// Preserves all other fields (projects, onboarding state, etc). Atomic.
fn apply_oauth_account_to_claude_config(blob: &serde_json::Value) -> Result<()> {
    let path = BaseDirs::new()
        .map(|b| b.home_dir().join(".claude.json"))
        .ok_or_else(|| anyhow!("cannot determine ~/.claude.json path"))?;
    let mut value: serde_json::Value = match fs::read_to_string(&path) {
        Ok(data) => {
            serde_json::from_str(&data).with_context(|| format!("parse {}", redact(&path)))?
        }
        // Missing file is fine — Claude Code will recreate on next launch.
        // We still write our oauthAccount so the next launch sees the right
        // identity from the start.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(anyhow!("read {}: {}", redact(&path), e)),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("oauthAccount".to_string(), blob.clone());
    } else {
        bail!("{} is not a JSON object", redact(&path));
    }
    let serialized = serde_json::to_string_pretty(&value).context("serialize claude config")?;
    // 0o600: `oauthAccount` carries email + org name + UUIDs. Even though
    // there's no bearer token in this file, world-readable identity leakage
    // is gratuitous on a multi-user box.
    atomic_write(&path, serialized.as_bytes(), Some(0o600))
        .with_context(|| format!("write {}", redact(&path)))?;
    Ok(())
}

pub fn list() -> Result<()> {
    // Pick up any token / plan changes that happened outside tku since the
    // last run, so the displayed plan and "active" marker reflect reality
    // instead of whatever was captured at `add` time.
    reconcile_live_creds();
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
    reconcile_live_creds();
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
