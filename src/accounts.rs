//! Multi-account support for Claude Code.
//!
//! tku stashes the live Claude Code login (`~/.claude/.credentials.json`, or
//! the login Keychain on macOS) under
//! `~/.config/tku/accounts/claude/<name>.credentials.json` and records every
//! swap in a registry file, so historical usage records can be attributed to
//! the account that was active at the time.
//!
//! Design notes:
//! - Only the OAuth credentials are swapped. Everything else in `~/.claude/`
//!   (skills, CLAUDE.md, settings, hooks) stays shared.
//! - Account key (`id`) is `<account_uuid>@<org_uuid>` from the profile API,
//!   or the bare org UUID while the account UUID is unresolved. Names are aliases.
//! - Tokens Claude Code rotates in the live login are copied back into the
//!   active account's stash once the profile API confirms their owner.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::atomic_write::atomic_write;
use crate::creds;
use crate::paths;
use crate::types::UsageRecord;

const TOOL_CLAUDE: &str = "claude";

/// Key of a resolved account. One person holds the same account UUID in every org.
pub fn account_key(account_uuid: &str, org_uuid: &str) -> String {
    format!("{account_uuid}@{org_uuid}")
}

// --- Types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "AccountDe")]
pub struct Account {
    pub name: String,
    /// Account key: `<account_uuid>@<org_uuid>`, or the org UUID while unresolved.
    pub id: String,
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

/// Version 1 registries carry no `id`; it falls back to `org_uuid`.
#[derive(Deserialize)]
struct AccountDe {
    name: String,
    #[serde(default)]
    id: Option<String>,
    org_uuid: String,
    added_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
    #[serde(default)]
    subscription_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
    #[serde(default)]
    oauth_account: Option<serde_json::Value>,
}

impl From<AccountDe> for Account {
    fn from(de: AccountDe) -> Self {
        Self {
            name: de.name,
            id: de.id.unwrap_or_else(|| de.org_uuid.clone()),
            org_uuid: de.org_uuid,
            added_at: de.added_at,
            last_used_at: de.last_used_at,
            subscription_type: de.subscription_type,
            rate_limit_tier: de.rate_limit_tier,
            oauth_account: de.oauth_account,
        }
    }
}

impl Account {
    /// True while the account UUID is unknown and `id` holds the org UUID.
    pub fn is_provisional(&self) -> bool {
        self.id == self.org_uuid
    }

    /// First 8 characters of the account UUID, or of the org UUID while provisional.
    pub fn short_id(&self) -> String {
        short_org(
            self.id
                .split_once('@')
                .map_or(self.id.as_str(), |(account, _)| account),
        )
    }

    pub fn blob_account_uuid(&self) -> Option<&str> {
        self.oauth_account
            .as_ref()?
            .get("accountUuid")
            .and_then(|v| v.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "SwitchEntryDe")]
pub struct SwitchEntry {
    pub at: DateTime<Utc>,
    pub id: String,
    pub name: String,
    pub source: SwitchSource,
}

/// Version 1 switch entries carry `org_uuid` in place of `id`.
#[derive(Deserialize)]
struct SwitchEntryDe {
    at: DateTime<Utc>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    org_uuid: Option<String>,
    name: String,
    source: SwitchSource,
}

impl TryFrom<SwitchEntryDe> for SwitchEntry {
    type Error = &'static str;

    fn try_from(de: SwitchEntryDe) -> std::result::Result<Self, Self::Error> {
        let id = de
            .id
            .or(de.org_uuid)
            .ok_or("switch entry has neither `id` nor `org_uuid`")?;
        Ok(Self {
            at: de.at,
            id,
            name: de.name,
            source: de.source,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchSource {
    /// User ran `tku account use`
    Explicit,
    /// Login switched outside tku, recorded by `tku account add`
    Implicit,
    /// First-run auto-registration
    Bootstrap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub switch_log: Vec<SwitchEntry>,
}

const REGISTRY_VERSION: u32 = 2;

/// Files without a `version` field predate version 2.
fn default_version() -> u32 {
    1
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            accounts: Vec::new(),
            switch_log: Vec::new(),
        }
    }
}

impl Registry {
    pub fn find_by_name(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.name == name)
    }

    pub fn find_by_id(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Rewrite `old` to `new` on the matching account and its switch entries.
    pub fn rekey_ids(&mut self, old: &str, new: &str) -> bool {
        if old == new {
            return false;
        }
        let mut changed = false;
        for a in self.accounts.iter_mut().filter(|a| a.id == old) {
            a.id = new.to_string();
            changed = true;
        }
        for e in self.switch_log.iter_mut().filter(|e| e.id == old) {
            e.id = new.to_string();
            changed = true;
        }
        changed
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

    /// Name of the account owning a record: the account whose id equals `tag`,
    /// else the switch-log owner at `ts`.
    pub fn owner_of(&self, tag: Option<&str>, ts: DateTime<Utc>) -> Option<&str> {
        tag.and_then(|t| self.find_by_id(t))
            .map(|a| a.name.as_str())
            .or_else(|| self.account_at(ts).map(|e| e.name.as_str()))
    }
}

// --- Paths ---

fn registry_path(tool: &str) -> Option<PathBuf> {
    paths::registry_file(tool)
}

pub fn stashed_creds_path(tool: &str, name: &str) -> Option<PathBuf> {
    paths::accounts_dir(tool).map(|d| d.join(format!("{name}.credentials.json")))
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
    // also carries the canonical account id → name mapping, so a torn write
    // here would corrupt attribution for every subsequent run.
    atomic_write(&path, data.as_bytes(), Some(0o600))
        .with_context(|| format!("write {}", redact(&path)))?;
    Ok(())
}

// --- Credential inspection ---

#[derive(Clone)]
struct CredsInfo {
    access_token: Option<String>,
    expires_at: Option<u64>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
}

// Hand-written Debug that omits the access token: any `{:?}` print of a
// CredsInfo (or a struct that contains one) would otherwise leak the bearer
// token into logs or error chains.
impl std::fmt::Debug for CredsInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredsInfo")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("subscription_type", &self.subscription_type)
            .field("rate_limit_tier", &self.rate_limit_tier)
            .finish()
    }
}

/// Identity fields of a `.claude.json` `oauthAccount` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OauthIds {
    pub account_uuid: Option<String>,
    pub org_uuid: Option<String>,
}

impl OauthIds {
    /// None when the blob carries neither field.
    fn from_blob(blob: &serde_json::Value) -> Option<Self> {
        let field = |k: &str| blob.get(k).and_then(|v| v.as_str()).map(String::from);
        let ids = Self {
            account_uuid: field("accountUuid"),
            org_uuid: field("organizationUuid"),
        };
        (ids != Self::default()).then_some(ids)
    }

    /// Account key, when the block carries both UUIDs.
    pub fn id(&self) -> Option<String> {
        Some(account_key(
            self.account_uuid.as_deref()?,
            self.org_uuid.as_deref()?,
        ))
    }
}

/// Live login's ids from `~/.claude.json`'s `oauthAccount` block. Only as
/// fresh as Claude Code's last write to that file. No network.
pub(crate) fn current_claude_oauth_ids() -> Option<OauthIds> {
    OauthIds::from_blob(&read_current_claude_oauth_account()?)
}

/// `oauthAccount` ids from the Claude config at `path`, for an isolated config dir.
pub(crate) fn oauth_ids_at(path: &Path) -> Option<OauthIds> {
    OauthIds::from_blob(&read_oauth_account_at(path)?)
}

/// Best-effort hint for callers that tolerate an unknown login.
fn read_current_claude_creds_info() -> Option<CredsInfo> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    match read_live() {
        Ok(live) => live.map(|(_, info)| info),
        Err(e) => {
            if creds::is_locked(&e) && !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("warning: macOS Keychain is locked; live login unknown");
            }
            None
        }
    }
}

/// Live credentials blob and its parsed fields; `None` when there is no usable login.
fn read_live() -> Result<Option<(Vec<u8>, CredsInfo)>> {
    let Some(bytes) = creds::live()?.read()? else {
        return Ok(None);
    };
    Ok(parse_creds_info(&bytes).map(|info| (bytes, info)))
}

fn parse_creds_info(bytes: &[u8]) -> Option<CredsInfo> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    // Credentials without the OAuth object are unusable for any purpose.
    value.get("claudeAiOauth")?;
    let access_token = value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
        .map(String::from);
    let expires_at = value
        .pointer("/claudeAiOauth/expiresAt")
        .and_then(|v| v.as_u64());
    let subscription_type = value
        .pointer("/claudeAiOauth/subscriptionType")
        .and_then(|v| v.as_str())
        .map(String::from);
    let rate_limit_tier = value
        .pointer("/claudeAiOauth/rateLimitTier")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(CredsInfo {
        access_token,
        expires_at,
        subscription_type,
        rate_limit_tier,
    })
}

/// Write `data` to `path` atomically with mode 0600 from the moment of
/// creation (no post-hoc chmod window).
fn write_secure(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    atomic_write(path, data, Some(0o600))
}

fn stashed_access_token(name: &str) -> Option<String> {
    let bytes = fs::read(stashed_creds_path(TOOL_CLAUDE, name)?).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .pointer("/claudeAiOauth/accessToken")?
        .as_str()
        .map(String::from)
}

fn stash_creds_bytes(tool: &str, name: &str, data: &[u8]) -> Result<PathBuf> {
    let dst = stashed_creds_path(tool, name).context("cannot determine stash path")?;
    if let Some(parent) = dst.parent() {
        create_stash_parent(parent).with_context(|| format!("create {}", redact(parent)))?;
    }
    write_secure(&dst, data).with_context(|| format!("write {}", redact(&dst)))?;
    Ok(dst)
}

// --- Bootstrap / reconciliation ---

/// Cheap reconciliation pass: keep the active account's vault and plan
/// metadata current with whatever Claude Code wrote to the live login since
/// our last run. This covers the gap where the user runs `claude /login` (or
/// Claude refreshes the token in the background) without subsequently
/// invoking `tku account use` — the fresh tokens would otherwise sit only in
/// the live login and be lost on the next swap-back.
///
/// Called on every `tku` invocation except for `account use` (which runs
/// its own snapshot inline) and `account` subcommands that don't touch
/// credentials. No-op when the live token hasn't changed. The live login's
/// owner is confirmed through the profile API, so without `network` (`tku bar`
/// and friends) the snapshot waits for the next `tku sub`.
pub fn reconcile_live_creds(network: bool) {
    if !network {
        return;
    }
    let mut registry = load_registry(TOOL_CLAUDE);
    if registry.accounts.is_empty() {
        return;
    }
    // Cheap check up-front: if the live token already matches the active
    // vault, skip both the identity verification and the registry write.
    let Some(active) = registry.latest_switch().cloned() else {
        return;
    };
    let Some(idx) = registry.accounts.iter().position(|a| a.id == active.id) else {
        return;
    };
    let account = &registry.accounts[idx];
    let Some(live_token) = read_current_claude_creds_info().and_then(|i| i.access_token) else {
        return;
    };
    if stashed_access_token(&account.name).as_deref() == Some(live_token.as_str()) {
        if account.is_provisional() {
            let Ok(identity) =
                crate::subscription::fetch_account_identity(&live_token, Some(&account.id))
            else {
                return;
            };
            if confirm_or_backfill(&mut registry, idx, &identity, true)
                && !registry.accounts[idx].is_provisional()
            {
                if let Err(e) = save_registry(TOOL_CLAUDE, &registry) {
                    eprintln!(
                        "warning: failed to persist registry after resolving account id: {e}"
                    );
                }
            }
        }
        return;
    }
    match snapshot_live_into_active_vault(&mut registry, network) {
        Ok(true) => {
            if let Err(e) = save_registry(TOOL_CLAUDE, &registry) {
                eprintln!("warning: failed to persist registry after snapshot: {e}");
            }
        }
        Ok(false) => {}
        Err(e) => eprintln!("warning: could not snapshot live credentials: {e}"),
    }
}

/// `tku sub` bootstrap: registers the live login as "default" when the
/// registry is empty, resolving its identity through the profile API and
/// anchoring the first switch entry to the earliest record. Kept off the
/// per-command path so `tku bar` and friends never block on the network.
pub fn bootstrap_via_profile_api(claude_records: &[&UsageRecord]) {
    let Some((bytes, info)) = read_live().ok().flatten() else {
        return;
    };
    let mut registry = load_registry(TOOL_CLAUDE);
    let now_ms = Utc::now().timestamp_millis() as u64;
    let Some(token) = profile_bootstrap_token(&registry, &info, now_ms) else {
        return;
    };
    let identity = match crate::subscription::fetch_account_identity(token, None) {
        Ok(identity) => identity,
        Err(e) => {
            eprintln!(
                "warning: could not register the current account, the profile API lookup \
                 failed ({e:#}). Re-run `tku sub` once it is reachable."
            );
            return;
        }
    };
    let id = identity.id();
    register_default(
        &mut registry,
        earliest_record(claude_records),
        info,
        id.clone(),
        identity.org_uuid,
        Some(identity.oauth_account),
    );
    persist_default(&registry, &bytes);
    crate::subscription::adopt_unknown_snapshots(&id);
}

/// Token to resolve the first account with, or None when there is nothing to
/// do: registry already populated, no token, or a token `tku sub` is about to
/// reject as expired anyway.
fn profile_bootstrap_token<'a>(
    registry: &Registry,
    info: &'a CredsInfo,
    now_ms: u64,
) -> Option<&'a str> {
    if !registry.accounts.is_empty() {
        return None;
    }
    if info.expires_at.is_some_and(|exp| now_ms > exp) {
        return None;
    }
    info.access_token.as_deref()
}

fn earliest_record(claude_records: &[&UsageRecord]) -> DateTime<Utc> {
    claude_records
        .iter()
        .map(|r| r.timestamp)
        .min()
        .unwrap_or_else(Utc::now)
}

fn register_default(
    registry: &mut Registry,
    anchor: DateTime<Utc>,
    info: CredsInfo,
    id: String,
    org_uuid: String,
    oauth_account: Option<serde_json::Value>,
) {
    let now = Utc::now();
    registry.accounts.push(Account {
        name: "default".to_string(),
        id: id.clone(),
        org_uuid,
        added_at: now,
        last_used_at: now,
        subscription_type: info.subscription_type,
        rate_limit_tier: info.rate_limit_tier,
        oauth_account,
    });
    registry.switch_log.push(SwitchEntry {
        at: anchor,
        id,
        name: "default".to_string(),
        source: SwitchSource::Bootstrap,
    });
}

fn persist_default(registry: &Registry, bytes: &[u8]) {
    if let Err(e) = stash_creds_bytes(TOOL_CLAUDE, "default", bytes) {
        eprintln!("warning: failed to stash credentials for 'default': {e}");
    }
    if let Err(e) = save_registry(TOOL_CLAUDE, registry) {
        eprintln!("warning: failed to persist account registry: {e}");
    }
}

fn short_org(org: &str) -> String {
    org.chars().take(8).collect()
}

// --- Account commands ---

pub(crate) fn validate_name(name: &str) -> Result<()> {
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
    let live = creds::live()?;
    let data = live
        .read()
        .with_context(|| {
            format!(
                "Cannot read the live Claude Code login ({})",
                live.describe()
            )
        })?
        .ok_or_else(|| {
            anyhow!(
                "No Claude Code login found ({}). Sign in to Claude Code first.",
                live.describe()
            )
        })?;
    let info = parse_creds_info(&data)
        .ok_or_else(|| anyhow!("Live credentials ({}) are unparseable", live.describe()))?;
    // Modern Claude Code creds files don't carry `organizationUuid`, so we
    // resolve identity via the profile API using the live access token.
    // We also stash the resulting `oauthAccount` blob so `use_account` can
    // keep `.claude.json` in sync with the swapped credentials — Claude
    // Code only writes that file at first onboarding, never on /login.
    let token = info.access_token.clone().ok_or_else(|| {
        anyhow!(
            "Credentials file has no access token. Sign in to Claude Code first, then try again."
        )
    })?;
    let identity = crate::subscription::fetch_account_identity(&token, None)
        .context("Could not fetch account identity from the Anthropic profile API")?;
    add_identity(name, &data, info, &token, identity, stashed_access_token)
}

/// Save the login `data` whose owner is `identity` as account `name`.
fn add_identity(
    name: &str,
    data: &[u8],
    info: CredsInfo,
    token: &str,
    identity: crate::subscription::AccountIdentity,
    stash_token: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    let mut registry = load_registry(TOOL_CLAUDE);
    if registry.find_by_name(name).is_some() {
        bail!(
            "Account '{name}' already exists. Use `tku account rename` to rename or `tku account remove` to delete."
        );
    }
    let already_saved = |existing: &str| {
        let email = identity
            .oauth_account
            .get("emailAddress")
            .and_then(|v| v.as_str())
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
        anyhow!(
            "This Claude login{email} is already saved as '{existing}'.\n\
             To save a different account, log out of Claude Code and log back in with the other one first:\n  \
               claude /logout\n  \
               claude /login\n  \
               tku account add {name}"
        )
    };
    let warn_provisional = match add_outcome(&registry, &identity, token, stash_token) {
        AddOutcome::Duplicate { name } => return Err(already_saved(&name)),
        AddOutcome::ResolveProvisional { name } => {
            if let Some(account) = registry.accounts.iter_mut().find(|a| a.name == name) {
                account.oauth_account = Some(identity.oauth_account.clone());
            }
            resolve_provisional(&mut registry, &name, &identity.account_uuid);
            save_registry(TOOL_CLAUDE, &registry)?;
            return Err(already_saved(&name));
        }
        AddOutcome::Register { warn_provisional } => warn_provisional,
    };

    stash_creds_bytes(TOOL_CLAUDE, name, data)?;

    let id = identity.id();
    let now = Utc::now();
    let account = Account {
        name: name.to_string(),
        id: id.clone(),
        org_uuid: identity.org_uuid,
        added_at: now,
        last_used_at: now,
        subscription_type: info.subscription_type,
        rate_limit_tier: info.rate_limit_tier,
        oauth_account: Some(identity.oauth_account),
    };
    let (short_id, short_org_uuid) = (account.short_id(), short_org(&account.org_uuid));
    registry.accounts.push(account);
    // Append a switch-log entry so consumers that infer the active account
    // from `latest_switch()` (list, current, subscription keying) see the
    // newly-added account as live. This is correct by construction — `add`
    // can only register the account that owns the currently-live creds.
    // Tagged Implicit because the actual swap happened outside tku at an
    // unknown earlier time; we anchor to `now` since we can't recover it.
    let needs_switch_entry = registry.latest_switch().map(|s| s.id != id).unwrap_or(true);
    if needs_switch_entry {
        registry.switch_log.push(SwitchEntry {
            at: now,
            id: id.clone(),
            name: name.to_string(),
            source: SwitchSource::Implicit,
        });
    }
    save_registry(TOOL_CLAUDE, &registry)?;
    if registry.accounts.len() == 1 {
        crate::subscription::adopt_unknown_snapshots(&id);
    }

    eprintln!("Registered account '{name}' (id: {short_id}, org: {short_org_uuid}).");
    if let Some(other) = warn_provisional {
        eprintln!(
            "warning: '{other}' is saved for the same organization, but its account id is not \
             resolved yet. If it is the same login, run `tku account use {other}` to resolve it, \
             then remove one of the two."
        );
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum AddOutcome {
    /// The login is already saved under `name`.
    Duplicate { name: String },
    /// The provisional account `name` holds this login's token.
    ResolveProvisional { name: String },
    /// Save as a new account; `warn_provisional` names an unresolved account in the same org.
    Register { warn_provisional: Option<String> },
}

fn add_outcome(
    registry: &Registry,
    identity: &crate::subscription::AccountIdentity,
    live_token: &str,
    stash_token: impl Fn(&str) -> Option<String>,
) -> AddOutcome {
    if let Some(existing) = registry.find_by_id(&identity.id()) {
        return AddOutcome::Duplicate {
            name: existing.name.clone(),
        };
    }
    let Some(provisional) = registry
        .accounts
        .iter()
        .find(|a| a.is_provisional() && a.org_uuid == identity.org_uuid)
    else {
        return AddOutcome::Register {
            warn_provisional: None,
        };
    };
    let name = provisional.name.clone();
    if stash_token(&name).as_deref() == Some(live_token) {
        AddOutcome::ResolveProvisional { name }
    } else {
        AddOutcome::Register {
            warn_provisional: Some(name),
        }
    }
}

/// Rekey the provisional account `name` to `account_uuid` in its org. The caller saves the
/// registry. Returns false and leaves `registry` unchanged when the rekey cannot be persisted.
fn resolve_provisional(registry: &mut Registry, name: &str, account_uuid: &str) -> bool {
    let Some((old, new)) = registry
        .find_by_name(name)
        .filter(|a| a.is_provisional())
        .map(|a| (a.id.clone(), account_key(account_uuid, &a.org_uuid)))
    else {
        return false;
    };
    if let Some(other) = registry.find_by_id(&new) {
        eprintln!(
            "warning: account id {} already belongs to '{}'; leaving '{name}' unresolved",
            other.short_id(),
            other.name
        );
        return false;
    }
    let mut store = crate::storage::default_storage();
    // Rekey a copy: callers that save the registry for other reasons must not persist the new id.
    let mut rekeyed = registry.clone();
    rekey(&mut rekeyed, store.as_mut(), &old, &new);
    if !store.flush() {
        eprintln!(
            "tku: warning: record cache not written; '{name}' stays unresolved until the next run"
        );
        return false;
    }
    *registry = rekeyed;
    true
}

/// True when `identity` is the account at `idx`. A provisional account is
/// resolved to `identity` only when it is the sole provisional account of that
/// org and `live_is_vault` says the live token is the one in its stash.
fn confirm_or_backfill(
    registry: &mut Registry,
    idx: usize,
    identity: &crate::subscription::AccountIdentity,
    live_is_vault: bool,
) -> bool {
    let account = &registry.accounts[idx];
    if identity.id() == account.id {
        return true;
    }
    if !live_is_vault || !account.is_provisional() || account.org_uuid != identity.org_uuid {
        return false;
    }
    let same_org = registry
        .accounts
        .iter()
        .filter(|a| a.is_provisional() && a.org_uuid == identity.org_uuid)
        .count();
    if same_org != 1 {
        return false;
    }
    let name = account.name.clone();
    resolve_provisional(registry, &name, &identity.account_uuid)
}

/// Account without a stored `oauthAccount` blob that the live `ids` belong to:
/// the one with that account id, or the sole provisional account of that org
/// whose stash holds `live_token`.
fn capture_target(
    registry: &Registry,
    ids: &OauthIds,
    live_token: Option<&str>,
    stash_token: impl Fn(&str) -> Option<String>,
) -> Option<usize> {
    if let Some(id) = ids.id() {
        if let Some(idx) = registry.accounts.iter().position(|a| a.id == id) {
            return registry.accounts[idx]
                .oauth_account
                .is_none()
                .then_some(idx);
        }
    }
    let org = ids.org_uuid.as_deref()?;
    let live_token = live_token?;
    let mut same_org = registry
        .accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_provisional() && a.org_uuid == org);
    let (idx, account) = same_org.next()?;
    if same_org.next().is_some() || account.oauth_account.is_some() {
        return None;
    }
    (stash_token(&account.name).as_deref() == Some(live_token)).then_some(idx)
}

pub fn use_account(name: &str) -> Result<()> {
    validate_name(name)?;

    let mut registry = load_registry(TOOL_CLAUDE);
    if registry.find_by_name(name).is_none() {
        bail!("Account '{name}' not found. Run `tku account list` to see available accounts.");
    }

    // Snapshot the live credentials back into the currently-active vault
    // entry. Claude Code rotates tokens on launch, refresh, and `/login`
    // without ever writing them back to our stash; without this step, the
    // next swap-back would clobber the live file with a stale token and
    // potentially an invalidated refresh token. Identity gated below; safe
    // to call before deciding which account to switch to.
    if let Err(e) = snapshot_live_into_active_vault(&mut registry, true) {
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

    // Pre-swap migration: if `.claude.json:oauthAccount` matches a registered
    // account that hasn't had its identity stored yet, capture it now —
    // before we overwrite the file. This catches the original-onboarding
    // account, whose stashed access token may already be too old to use the
    // profile API as a fallback path.
    if let Some(current_oauth) = read_current_claude_oauth_account() {
        if let Some(ids) = OauthIds::from_blob(&current_oauth) {
            let live_token = read_current_claude_creds_info().and_then(|i| i.access_token);
            if let Some(idx) =
                capture_target(&registry, &ids, live_token.as_deref(), stashed_access_token)
            {
                // The blob may be stale; the post-swap profile call resolves the id.
                registry.accounts[idx].oauth_account = Some(current_oauth);
            }
        }
    }

    let data = fs::read(&src).with_context(|| format!("read {}", redact(&src)))?;
    let live = creds::live()?;
    live.write(&data)
        .with_context(|| format!("write {}", live.describe()))?;

    // Re-read the (possibly migrated) account record before deciding whether
    // to backfill via API.
    let account = registry
        .find_by_name(name)
        .cloned()
        .expect("account existed at top of function");

    // Backfill the `oauthAccount` blob and the account id for entries that
    // lack them. After the creds swap above, the live access token belongs to
    // this account, so the profile API will identify it correctly. If the
    // stashed token is expired, this will 401 — we warn and continue.
    // Pass the in-memory bytes we just wrote (avoids a re-read TOCTOU).
    let mut oauth_account = account.oauth_account.clone();
    if oauth_account.is_none() || account.is_provisional() {
        match fetch_identity_from_creds_bytes(&data, &account.id) {
            Ok(identity) => {
                if let Some(a) = registry.accounts.iter_mut().find(|a| a.name == name) {
                    a.oauth_account = Some(identity.oauth_account.clone());
                }
                if account.is_provisional() && identity.org_uuid == account.org_uuid {
                    resolve_provisional(&mut registry, name, &identity.account_uuid);
                }
                oauth_account = Some(identity.oauth_account);
            }
            Err(e) if oauth_account.is_none() => {
                eprintln!(
                    "warning: could not refresh stored identity for '{name}' (Claude Code's UI will display the previous account until you launch claude once and re-run this command): {e}"
                );
            }
            Err(_) => {}
        }
    }
    let account = registry
        .find_by_name(name)
        .cloned()
        .expect("account existed at top of function");

    if let Some(blob) = &oauth_account {
        if let Err(e) = apply_oauth_account_to_claude_config(blob) {
            eprintln!("warning: could not update ~/.claude.json: {e}");
        }
    }

    let now = Utc::now();
    registry.switch_log.push(SwitchEntry {
        at: now,
        id: account.id.clone(),
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
    Ok(())
}

/// Read the current `oauthAccount` object from `~/.claude.json`,
/// if present. Used by the pre-swap migration to capture the originally-
/// onboarded account's identity before it gets overwritten.
fn read_current_claude_oauth_account() -> Option<serde_json::Value> {
    read_oauth_account_at(&BaseDirs::new()?.home_dir().join(".claude.json"))
}

fn read_oauth_account_at(path: &Path) -> Option<serde_json::Value> {
    let data = fs::read(path).ok()?;
    let mut value: serde_json::Value = serde_json::from_slice(&data).ok()?;
    value.get_mut("oauthAccount").map(serde_json::Value::take)
}

/// Parse a credentials JSON blob and ask the profile API for its identity,
/// caching the profile under `cache_key`.
/// Takes raw bytes so callers can reuse data they've already loaded — avoids
/// a re-read TOCTOU window.
fn fetch_identity_from_creds_bytes(
    data: &[u8],
    cache_key: &str,
) -> Result<crate::subscription::AccountIdentity> {
    let value: serde_json::Value = serde_json::from_slice(data).context("parse credentials")?;
    let token = value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("credentials file has no access token"))?;
    crate::subscription::fetch_account_identity(token, Some(cache_key))
}

/// Snapshot the live Claude Code login back into the
/// currently-active vault entry, refreshing the registry's plan metadata at
/// the same time. Identity-gated: we only overwrite the vault if the live
/// creds demonstrably belong to the currently-active account.
///
/// Behaviour:
/// - No-op when live creds are absent, unparseable, or have no `accessToken`.
/// - No-op when the vault already holds an identical `accessToken` (the
///   common case — Claude hasn't rotated since our last snapshot).
/// - Identity check: the profile API must attribute the live creds to the
///   active account. Without `network` we skip quietly. On API failure we
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
fn snapshot_live_into_active_vault(registry: &mut Registry, network: bool) -> Result<bool> {
    let Some(active_entry) = registry.latest_switch().cloned() else {
        return Ok(false);
    };
    let Some(active_idx) = registry
        .accounts
        .iter()
        .position(|a| a.id == active_entry.id)
    else {
        // Active entry references an id we no longer have a vault for
        // (e.g. user ran `account remove --force` on the live account).
        // Nothing to snapshot into.
        return Ok(false);
    };
    let active_name = registry.accounts[active_idx].name.clone();

    let live = creds::live()?;
    let Some(live_bytes) = live
        .read()
        .with_context(|| format!("read {}", live.describe()))?
    else {
        return Ok(false);
    };
    let live_value: serde_json::Value = match serde_json::from_slice(&live_bytes) {
        Ok(v) => v,
        Err(_) => return Ok(false), // Claude rewriting mid-read; try again next swap.
    };
    let Some(live_token) = live_value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(|v| v.as_str())
    else {
        return Ok(false);
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
    let live_is_vault = vault_token.as_deref() == Some(live_token);
    if live_is_vault {
        return Ok(false); // Vault is already current.
    }

    if !network {
        return Ok(false); // Verified on the next `tku sub`.
    }
    // The live token may belong to another login; its profile is cached under its own id.
    let identity_ok = match crate::subscription::fetch_account_identity(live_token, None) {
        Ok(identity) => confirm_or_backfill(registry, active_idx, &identity, false),
        Err(e) => {
            eprintln!(
                "warning: the profile API could not confirm the owner of the live credentials \
                 ({e}). Skipping vault snapshot for '{active_name}' — the next swap may \
                 restore a stale token."
            );
            return Ok(false);
        }
    };
    if !identity_ok {
        if registry.accounts[active_idx].is_provisional() {
            eprintln!(
                "warning: cannot confirm the live credentials belong to '{active_name}' \
                 (account id unresolved). Skipping vault snapshot; run `tku account use \
                 {active_name}` to resolve it."
            );
        } else {
            eprintln!(
                "warning: live credentials don't belong to the currently-active account \
                 '{active_name}'. Skipping vault snapshot to avoid cross-account contamination."
            );
        }
        return Ok(false);
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
    Ok(true)
}

/// Patch `~/.claude.json` so its `oauthAccount` key matches `blob`.
/// Preserves all other fields (projects, onboarding state, etc). Atomic.
fn apply_oauth_account_to_claude_config(blob: &serde_json::Value) -> Result<()> {
    let path = BaseDirs::new()
        .map(|b| b.home_dir().join(".claude.json"))
        .ok_or_else(|| anyhow!("cannot determine ~/.claude.json path"))?;
    apply_oauth_account_to_config(&path, blob)
}

/// Patch the Claude config at `path` so its `oauthAccount` key matches `blob`,
/// preserving all other fields (projects, onboarding state, etc). Atomic.
/// Used both for the global `~/.claude.json` and for an isolated
/// `$CLAUDE_CONFIG_DIR/.claude.json` seeded by `account exec`.
pub(crate) fn apply_oauth_account_to_config(path: &Path, blob: &serde_json::Value) -> Result<()> {
    let mut value: serde_json::Value = match fs::read_to_string(path) {
        Ok(data) => {
            serde_json::from_str(&data).with_context(|| format!("parse {}", redact(path)))?
        }
        // Missing file is fine — Claude Code will recreate on next launch.
        // We still write our oauthAccount so the next launch sees the right
        // identity from the start.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(anyhow!("read {}: {}", redact(path), e)),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert("oauthAccount".to_string(), blob.clone());
    } else {
        bail!("{} is not a JSON object", redact(path));
    }
    let serialized = serde_json::to_string_pretty(&value).context("serialize claude config")?;
    // 0o600: `oauthAccount` carries email + org name + UUIDs. Even though
    // there's no bearer token in this file, world-readable identity leakage
    // is gratuitous on a multi-user box.
    atomic_write(path, serialized.as_bytes(), Some(0o600))
        .with_context(|| format!("write {}", redact(path)))?;
    Ok(())
}

pub fn list() -> Result<()> {
    // Pick up any token / plan changes that happened outside tku since the
    // last run, so the displayed plan and "active" marker reflect reality
    // instead of whatever was captured at `add` time.
    reconcile_live_creds(true);
    let registry = load_registry(TOOL_CLAUDE);
    let active_id = registry.latest_switch().map(|s| s.id.as_str());

    if registry.accounts.is_empty() {
        eprintln!("No accounts registered.");
        eprintln!();
        eprintln!("Run `tku sub` once to auto-register your current account as 'default',");
        eprintln!("or `tku account add <name>` to register under a custom name.");
        return Ok(());
    }

    println!("Accounts (claude):");
    for a in &registry.accounts {
        let marker = if active_id == Some(a.id.as_str()) {
            "*"
        } else {
            " "
        };
        let plan = format_plan(a.subscription_type.as_deref(), a.rate_limit_tier.as_deref());
        println!(
            "  {} {:<20} id: {}  org: {}  {}",
            marker,
            a.name,
            a.short_id(),
            short_org(&a.org_uuid),
            plan
        );
    }
    println!();
    println!("* = currently active");
    Ok(())
}

pub fn current() -> Result<()> {
    reconcile_live_creds(true);
    let registry = load_registry(TOOL_CLAUDE);
    let info = read_current_claude_creds_info();

    match info {
        Some(info) => {
            let plan = format_plan(
                info.subscription_type.as_deref(),
                info.rate_limit_tier.as_deref(),
            );
            match registry.latest_switch() {
                Some(latest) => match registry.find_by_id(&latest.id) {
                    Some(a) => println!(
                        "Active: {} (id: {}, org: {}, {})",
                        a.name,
                        a.short_id(),
                        short_org(&a.org_uuid),
                        plan
                    ),
                    None => println!("Active: {} ({plan})", latest.name),
                },
                None => println!("Active: <unknown> ({plan})"),
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
    // the live login in an unsaved state — easy to do by accident.
    if !force && is_active(&registry, name) {
        bail!(
            "'{name}' is the currently-active account. Switch away first with `tku account use <other>`,\n\
             or pass --force to remove it anyway (the live login will stay in place but\n\
             won't be saved anywhere)."
        );
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

/// Move everything keyed by account id `old` to `new`: exec transcripts,
/// registry, record cache, snapshots and profile cache. Each step warns and
/// continues on failure. The caller saves the registry and flushes the store.
pub fn rekey(
    registry: &mut Registry,
    store: &mut dyn crate::storage::Storage,
    old: &str,
    new: &str,
) {
    match registry.find_by_id(old).map(|a| a.name.clone()) {
        Some(name) => {
            if let Err(e) = rekey_transcripts(&name, old, new) {
                eprintln!("tku: warning: could not move exec transcripts of '{name}': {e:#}");
            }
        }
        None => eprintln!("tku: warning: no account with id {old}; exec transcripts left in place"),
    }

    registry.rekey_ids(old, new);

    let old_dir = paths::spawn_transcripts_dir(TOOL_CLAUDE, old);
    let new_dir = paths::spawn_transcripts_dir(TOOL_CLAUDE, new);
    // Remapping while the old directory remains would count its files twice.
    let dirs = match (old_dir, new_dir) {
        (Some(o), Some(n)) if !o.exists() => Some((o, n)),
        _ => None,
    };
    store.rekey_account(
        old,
        new,
        dirs.as_ref().map(|(o, _)| o.as_path()),
        dirs.as_ref().map(|(_, n)| n.as_path()),
    );

    if let Err(e) = crate::subscription::rekey_snapshots(old, new) {
        eprintln!("tku: warning: could not move subscription snapshots: {e:#}");
    }
    if let Err(e) = rekey_profile_cache(old, new) {
        eprintln!("tku: warning: could not move the profile cache: {e:#}");
    }
}

/// Rekey every account whose cached blob names its account UUID from the org
/// UUID to `<account_uuid>@<org_uuid>`, then mark the registry version 2. Runs once per
/// registry; while another process holds the lock this one waits up to 30 s for it.
pub fn migrate_to_account_ids() {
    migrate_with_wait(std::time::Duration::from_secs(30));
}

fn migrate_with_wait(bound: std::time::Duration) {
    let pending = |r: &Registry| r.version < REGISTRY_VERSION && !r.accounts.is_empty();
    if !pending(&load_registry(TOOL_CLAUDE)) {
        return;
    }
    let deadline = std::time::Instant::now() + bound;
    let _lock = loop {
        // Dot prefix: account names cannot start with '.', so no `account exec` lock collides.
        let lock = crate::spawn::private_spawn_dir()
            .and_then(|dir| crate::spawn::SpawnLock::acquire(&dir.join(".migrate.lock")));
        match lock {
            Ok(Some(lock)) => break lock,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(None) => {
                eprintln!(
                    "tku: warning: another tku process holds the account id migration lock; \
                     migration skipped"
                );
                return;
            }
            Err(e) => {
                eprintln!("tku: warning: account id migration skipped: {e:#}");
                return;
            }
        }
    };
    let mut registry = load_registry(TOOL_CLAUDE);
    if !pending(&registry) {
        return;
    }

    let pairs: Vec<(String, String)> = registry
        .accounts
        .iter()
        .filter_map(|a| {
            let new = account_key(a.blob_account_uuid()?, &a.org_uuid);
            (new != a.id).then(|| (a.id.clone(), new))
        })
        .collect();
    if !pairs.is_empty() {
        let mut store = crate::storage::default_storage();
        for (old, new) in &pairs {
            rekey(&mut registry, store.as_mut(), old, new);
        }
        if !store.flush() {
            eprintln!(
                "tku: warning: record cache not written; the account id migration will retry on the next run"
            );
            return;
        }
    }

    registry.version = REGISTRY_VERSION;
    if let Err(e) = save_registry(TOOL_CLAUDE, &registry) {
        eprintln!("tku: warning: could not save the migrated account registry: {e:#}");
        return;
    }
    if !pairs.is_empty() {
        eprintln!(
            "tku: rekeyed {} account(s) from organization UUID to account and organization UUID",
            pairs.len()
        );
    }
}

fn rekey_transcripts(name: &str, old: &str, new: &str) -> Result<()> {
    let Some(root) = paths::transcripts_dir(TOOL_CLAUDE) else {
        return Ok(());
    };
    let (from, to) = (root.join(old), root.join(new));
    if !from.exists() {
        return Ok(());
    }
    // Shares the `account exec` lock so a session cannot link into `from` mid-rename.
    let parent = crate::spawn::private_spawn_dir()?;
    let Some(_lock) = crate::spawn::SpawnLock::acquire(&crate::spawn::lock_path(&parent, name))?
    else {
        eprintln!(
            "tku: warning: '{name}' is running in `tku account exec`; {} was not moved",
            redact(&from)
        );
        return Ok(());
    };
    if to.exists() {
        eprintln!(
            "tku: warning: both {} and {} exist; leaving both in place",
            redact(&from),
            redact(&to)
        );
        return Ok(());
    }
    fs::rename(&from, &to).with_context(|| format!("rename {} to {}", redact(&from), redact(&to)))
}

fn rekey_profile_cache(old: &str, new: &str) -> Result<()> {
    let (Some(from), Some(to)) = (
        paths::profile_cache_file(old),
        paths::profile_cache_file(new),
    ) else {
        return Ok(());
    };
    if !from.exists() {
        return Ok(());
    }
    if to.exists() {
        fs::remove_file(&from).with_context(|| format!("remove {}", redact(&from)))
    } else {
        fs::rename(&from, &to)
            .with_context(|| format!("rename {} to {}", redact(&from), redact(&to)))
    }
}

fn is_active(registry: &Registry, name: &str) -> bool {
    match (registry.latest_switch(), registry.find_by_name(name)) {
        (Some(latest), Some(account)) => latest.id == account.id,
        _ => false,
    }
}

fn format_plan(sub_type: Option<&str>, rate_tier: Option<&str>) -> String {
    crate::subscription::format_plan_label(sub_type, rate_tier)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{Account, Registry, SwitchEntry, SwitchSource};
    use crate::types::{Provider, UsageRecord};
    use chrono::{DateTime, Duration, TimeZone, Utc};

    pub(crate) fn ts(hour: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::hours(hour)
    }

    /// "alice" (acct-a@org-1) and "bob" (acct-b@org-1); alice from hour 0, bob from 1, alice from 2.
    pub(crate) fn same_org_registry() -> Registry {
        let account = |name: &str, id: &str| Account {
            name: name.into(),
            id: id.into(),
            org_uuid: "org-1".into(),
            added_at: ts(0),
            last_used_at: ts(0),
            subscription_type: None,
            rate_limit_tier: None,
            oauth_account: None,
        };
        let switch = |hour, name: &str, id: &str| SwitchEntry {
            at: ts(hour),
            id: id.into(),
            name: name.into(),
            source: SwitchSource::Explicit,
        };
        Registry {
            accounts: vec![
                account("alice", "acct-a@org-1"),
                account("bob", "acct-b@org-1"),
            ],
            switch_log: vec![
                switch(0, "alice", "acct-a@org-1"),
                switch(1, "bob", "acct-b@org-1"),
                switch(2, "alice", "acct-a@org-1"),
            ],
            ..Registry::default()
        }
    }

    pub(crate) fn record(tag: Option<&str>, timestamp: DateTime<Utc>) -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: "s".into(),
            timestamp,
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
            account_uuid: tag.map(String::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> CredsInfo {
        CredsInfo {
            access_token: Some("tok".to_string()),
            expires_at: Some(2_000),
            subscription_type: Some("max".to_string()),
            rate_limit_tier: None,
        }
    }

    #[test]
    fn profile_bootstrap_runs_only_for_an_empty_registry_with_a_live_token() {
        let empty = Registry::default();
        assert_eq!(profile_bootstrap_token(&empty, &info(), 1_000), Some("tok"));

        let mut populated = Registry::default();
        register_default(
            &mut populated,
            Utc::now(),
            info(),
            "acct-1@org-1".to_string(),
            "org-1".to_string(),
            None,
        );
        assert_eq!(profile_bootstrap_token(&populated, &info(), 1_000), None);

        let expired = info();
        assert_eq!(profile_bootstrap_token(&empty, &expired, 3_000), None);

        let no_token = CredsInfo {
            access_token: None,
            ..info()
        };
        assert_eq!(profile_bootstrap_token(&empty, &no_token, 1_000), None);
    }

    #[test]
    fn register_default_anchors_switch_to_earliest_record() {
        let mut registry = Registry::default();
        let anchor = DateTime::parse_from_rfc3339("2025-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        let oauth = serde_json::json!({ "organizationUuid": "org-1" });
        register_default(
            &mut registry,
            anchor,
            info(),
            "org-1".to_string(),
            "org-1".to_string(),
            Some(oauth.clone()),
        );
        let account = &registry.accounts[0];
        assert_eq!(account.name, "default");
        assert_eq!(account.id, "org-1");
        assert_eq!(account.org_uuid, "org-1");
        assert!(account.is_provisional());
        assert_eq!(account.subscription_type.as_deref(), Some("max"));
        assert_eq!(account.oauth_account, Some(oauth));
        let switch = registry.latest_switch().unwrap();
        assert_eq!(switch.at, anchor);
        assert_eq!(switch.id, "org-1");
        assert_eq!(switch.source, SwitchSource::Bootstrap);
    }

    #[test]
    fn register_default_sets_id_and_org_separately() {
        let mut registry = Registry::default();
        register_default(
            &mut registry,
            Utc::now(),
            info(),
            "acct-1@org-1".to_string(),
            "org-1".to_string(),
            None,
        );
        let account = &registry.accounts[0];
        assert_eq!(account.id, "acct-1@org-1");
        assert_eq!(account.org_uuid, "org-1");
        assert!(!account.is_provisional());
        assert_eq!(registry.latest_switch().unwrap().id, "acct-1@org-1");
    }

    fn identity(account_uuid: &str, org_uuid: &str) -> crate::subscription::AccountIdentity {
        crate::subscription::AccountIdentity {
            account_uuid: account_uuid.to_string(),
            org_uuid: org_uuid.to_string(),
            oauth_account: serde_json::json!({ "accountUuid": account_uuid }),
        }
    }

    /// Registry with one provisional account "work" in org-1.
    fn provisional_registry() -> Registry {
        let mut registry = Registry::default();
        register_default(
            &mut registry,
            Utc::now(),
            info(),
            "org-1".to_string(),
            "org-1".to_string(),
            None,
        );
        registry.accounts[0].name = "work".to_string();
        registry
    }

    fn stash(token: &'static str) -> impl Fn(&str) -> Option<String> {
        move |name| (name == "work").then(|| token.to_string())
    }

    #[test]
    fn add_outcome_resolves_provisional_on_token_match() {
        let registry = provisional_registry();
        assert_eq!(
            add_outcome(&registry, &identity("acct-1", "org-1"), "tok", stash("tok")),
            AddOutcome::ResolveProvisional {
                name: "work".to_string()
            }
        );
    }

    #[test]
    fn add_outcome_registers_new_same_org_account_with_warning() {
        let registry = provisional_registry();
        assert_eq!(
            add_outcome(
                &registry,
                &identity("acct-2", "org-1"),
                "tok-2",
                stash("tok")
            ),
            AddOutcome::Register {
                warn_provisional: Some("work".to_string())
            }
        );
        assert_eq!(
            add_outcome(
                &registry,
                &identity("acct-3", "org-2"),
                "tok-3",
                stash("tok")
            ),
            AddOutcome::Register {
                warn_provisional: None
            }
        );
    }

    #[test]
    fn add_outcome_rejects_duplicate_id() {
        let mut registry = provisional_registry();
        registry.rekey_ids("org-1", "acct-1@org-1");
        assert_eq!(
            add_outcome(
                &registry,
                &identity("acct-1", "org-1"),
                "tok-new",
                stash("tok")
            ),
            AddOutcome::Duplicate {
                name: "work".to_string()
            }
        );
        assert_eq!(
            add_outcome(
                &registry,
                &identity("acct-2", "org-1"),
                "tok-2",
                stash("tok")
            ),
            AddOutcome::Register {
                warn_provisional: None
            }
        );
    }

    #[test]
    fn add_outcome_registers_same_person_in_another_org() {
        let mut registry = provisional_registry();
        registry.rekey_ids("org-1", "acct-1@org-1");
        assert_eq!(
            add_outcome(
                &registry,
                &identity("acct-1", "org-2"),
                "tok-2",
                stash("tok")
            ),
            AddOutcome::Register {
                warn_provisional: None
            }
        );
    }

    #[test]
    fn account_key_pairs_account_and_org() {
        assert_eq!(account_key("acct-1", "org-1"), "acct-1@org-1");
        assert_eq!(identity("acct-1", "org-1").id(), "acct-1@org-1");
        assert_eq!(
            ids(Some("acct-1"), "org-1").id().as_deref(),
            Some("acct-1@org-1")
        );
        assert_eq!(ids(None, "org-1").id(), None);

        let mut registry = provisional_registry();
        let org = "0b5e2d7c-1111-4222-8333-944455556666";
        registry.accounts[0].org_uuid = org.to_string();
        registry.accounts[0].id = org.to_string();
        assert_eq!(registry.accounts[0].short_id(), "0b5e2d7c");
        registry.accounts[0].id = account_key("7f3a9c1e-aaaa-4bbb-8ccc-dddd00001111", org);
        assert_eq!(registry.accounts[0].short_id(), "7f3a9c1e");
    }

    #[test]
    fn v1_registry_loads_with_id_equal_org() {
        let json = r#"{
            "version": 1,
            "accounts": [{
                "name": "work",
                "org_uuid": "org-1",
                "added_at": "2025-01-01T00:00:00Z",
                "last_used_at": "2025-01-02T00:00:00Z",
                "oauth_account": { "accountUuid": "acct-1", "organizationUuid": "org-1" }
            }],
            "switch_log": [
                { "at": "2025-01-01T00:00:00Z", "org_uuid": "org-1", "name": "work", "source": "bootstrap" },
                { "at": "2025-01-02T00:00:00Z", "org_uuid": "org-2", "name": "other", "source": "implicit" }
            ]
        }"#;
        let registry: Registry = serde_json::from_str(json).unwrap();
        assert_eq!(registry.version, 1);
        let account = &registry.accounts[0];
        assert_eq!(account.id, "org-1");
        assert_eq!(account.org_uuid, "org-1");
        assert!(account.is_provisional());
        assert_eq!(account.blob_account_uuid(), Some("acct-1"));
        let ids: Vec<&str> = registry.switch_log.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["org-1", "org-2"]);
        assert!(registry.find_by_id("org-1").is_some());
    }

    #[test]
    fn v2_registry_roundtrips_id() {
        let at = DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let registry = Registry {
            version: REGISTRY_VERSION,
            accounts: vec![Account {
                name: "work".to_string(),
                id: "acct-1@org-1".to_string(),
                org_uuid: "org-1".to_string(),
                added_at: at,
                last_used_at: at,
                subscription_type: None,
                rate_limit_tier: None,
                oauth_account: None,
            }],
            switch_log: vec![SwitchEntry {
                at,
                id: "acct-1@org-1".to_string(),
                name: "work".to_string(),
                source: SwitchSource::Explicit,
            }],
        };
        let json = serde_json::to_string(&registry).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["accounts"][0]["id"], "acct-1@org-1");
        assert_eq!(value["switch_log"][0]["id"], "acct-1@org-1");
        assert!(value["switch_log"][0].get("org_uuid").is_none());

        let back: Registry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, 2);
        assert_eq!(back.accounts[0].id, "acct-1@org-1");
        assert_eq!(back.accounts[0].org_uuid, "org-1");
        assert!(!back.accounts[0].is_provisional());
        assert_eq!(back.switch_log[0].id, "acct-1@org-1");
    }

    #[test]
    fn rekey_ids_rewrites_account_and_its_switch_entries_only() {
        let mut registry = Registry::default();
        register_default(
            &mut registry,
            Utc::now(),
            info(),
            "org-1".to_string(),
            "org-1".to_string(),
            None,
        );
        register_default(
            &mut registry,
            Utc::now(),
            info(),
            "org-2".to_string(),
            "org-2".to_string(),
            None,
        );
        registry.accounts[1].name = "other".to_string();

        assert!(registry.rekey_ids("org-1", "acct-1@org-1"));
        assert_eq!(registry.accounts[0].id, "acct-1@org-1");
        assert_eq!(registry.accounts[0].org_uuid, "org-1");
        assert_eq!(registry.accounts[1].id, "org-2");
        assert_eq!(registry.switch_log[0].id, "acct-1@org-1");
        assert_eq!(registry.switch_log[1].id, "org-2");

        assert!(!registry.rekey_ids("org-1", "acct-1@org-1"));
        assert!(!registry.rekey_ids("missing", "acct-9"));
    }

    /// "default" (acct-1) and "other" (acct-2) in org-1, "other" switched to last.
    fn two_account_registry() -> Registry {
        let mut registry = Registry::default();
        for id in ["acct-1@org-1", "acct-2@org-1"] {
            register_default(
                &mut registry,
                Utc::now(),
                info(),
                id.to_string(),
                "org-1".to_string(),
                None,
            );
        }
        registry.accounts[1].name = "other".to_string();
        registry.switch_log[1].name = "other".to_string();
        registry
    }

    #[test]
    fn remove_refuses_the_active_account_without_force() {
        let (dir, home) = rekey_home("remove-active");
        save_registry(TOOL_CLAUDE, &two_account_registry()).unwrap();
        let stash = stash_creds_bytes(TOOL_CLAUDE, "other", b"{}").unwrap();
        let registry_file = paths::registry_file(TOOL_CLAUDE).unwrap();
        let before = fs::read_to_string(&registry_file).unwrap();

        let err = remove("other", false).unwrap_err();

        assert!(err.to_string().contains("currently-active"));
        assert_eq!(fs::read_to_string(&registry_file).unwrap(), before);
        assert_eq!(fs::read(&stash).unwrap(), b"{}");

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_active_follows_the_latest_switch() {
        let mut registry = Registry::default();
        register_default(
            &mut registry,
            Utc::now(),
            info(),
            "org-1".to_string(),
            "org-1".to_string(),
            None,
        );
        register_default(
            &mut registry,
            Utc::now(),
            info(),
            "org-1".to_string(),
            "org-1".to_string(),
            None,
        );
        registry.accounts[0].id = "acct-1@org-1".to_string();
        registry.accounts[1].id = "acct-2@org-1".to_string();
        registry.accounts[1].name = "other".to_string();
        registry.switch_log.clear();
        assert!(!is_active(&registry, "default"));

        registry.switch_log.push(SwitchEntry {
            at: Utc::now(),
            id: "acct-2@org-1".to_string(),
            name: "other".to_string(),
            source: SwitchSource::Explicit,
        });
        assert!(is_active(&registry, "other"));
        assert!(!is_active(&registry, "default"));
        assert!(!is_active(&registry, "missing"));
    }

    #[test]
    fn default_registry_is_version_2() {
        assert_eq!(Registry::default().version, 2);
    }

    /// `provisional_registry` plus a second provisional account "side" in org-1.
    fn two_provisionals() -> Registry {
        let mut registry = provisional_registry();
        let mut side = registry.accounts[0].clone();
        side.name = "side".to_string();
        registry.accounts.push(side);
        registry
    }

    #[test]
    fn resolve_provisional_refuses_when_id_taken() {
        let mut registry = two_provisionals();
        registry.accounts[1].id = "acct-1@org-1".to_string();
        assert!(!resolve_provisional(&mut registry, "work", "acct-1"));
        assert_eq!(registry.accounts[0].id, "org-1");
        assert_eq!(registry.latest_switch().unwrap().id, "org-1");
    }

    #[test]
    fn resolve_provisional_rekeys_registry_and_store() {
        let (dir, home) = rekey_home("resolve");
        seed_migration(2);
        let mut registry = load_registry(TOOL_CLAUDE);

        assert!(resolve_provisional(&mut registry, "side", "acct-2"));

        let side = registry.find_by_name("side").unwrap();
        assert_eq!(
            (side.id.as_str(), side.org_uuid.as_str()),
            ("acct-2@org-2", "org-2")
        );
        assert_eq!(registry.latest_switch().unwrap().id, "acct-2@org-2");
        assert!(registry.find_by_name("work").unwrap().is_provisional());
        assert_eq!(
            stored_records(),
            [
                ("m1".to_string(), "org-1".to_string()),
                ("m2".to_string(), "acct-2@org-2".to_string()),
                (AGG_ORG.to_string(), "org-1".to_string()),
            ]
        );
        assert_eq!(snapshot_keys(), ["acct-2@org-2", "org-1"]);
        assert!(!paths::profile_cache_file("org-2").unwrap().exists());
        assert!(paths::profile_cache_file("acct-2@org-2").unwrap().exists());

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_or_backfill_accepts_matching_id() {
        let mut registry = provisional_registry();
        registry.rekey_ids("org-1", "acct-1@org-1");
        assert!(confirm_or_backfill(
            &mut registry,
            0,
            &identity("acct-1", "org-1"),
            false
        ));
        assert!(!confirm_or_backfill(
            &mut registry,
            0,
            &identity("acct-2", "org-1"),
            true
        ));
    }

    #[test]
    fn confirm_or_backfill_resolves_sole_provisional_holding_the_live_token() {
        let (dir, home) = rekey_home("confirm");
        let mut registry = provisional_registry();
        assert!(confirm_or_backfill(
            &mut registry,
            0,
            &identity("acct-1", "org-1"),
            true
        ));
        assert_eq!(registry.accounts[0].id, "acct-1@org-1");
        assert_eq!(registry.latest_switch().unwrap().id, "acct-1@org-1");
        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_or_backfill_rejects_org_mismatch() {
        let mut registry = provisional_registry();
        assert!(!confirm_or_backfill(
            &mut registry,
            0,
            &identity("acct-1", "org-2"),
            true
        ));
        assert!(registry.accounts[0].is_provisional());
    }

    #[test]
    fn confirm_or_backfill_rejects_ambiguous_same_org_provisionals() {
        let mut registry = two_provisionals();
        assert!(!confirm_or_backfill(
            &mut registry,
            0,
            &identity("acct-1", "org-1"),
            true
        ));
        assert!(registry.accounts.iter().all(Account::is_provisional));
    }

    #[test]
    fn confirm_or_backfill_rejects_when_live_token_is_not_the_vault() {
        let mut registry = provisional_registry();
        assert!(!confirm_or_backfill(
            &mut registry,
            0,
            &identity("acct-1", "org-1"),
            false
        ));
        assert!(registry.accounts[0].is_provisional());
    }

    fn ids(account_uuid: Option<&str>, org_uuid: &str) -> OauthIds {
        OauthIds {
            account_uuid: account_uuid.map(String::from),
            org_uuid: Some(org_uuid.to_string()),
        }
    }

    #[test]
    fn capture_target_matches_account_id() {
        let mut registry = two_provisionals();
        registry.accounts[1].id = "acct-2@org-1".to_string();
        let live = ids(Some("acct-2"), "org-1");
        assert_eq!(capture_target(&registry, &live, None, stash("x")), Some(1));
        registry.accounts[1].oauth_account = Some(serde_json::json!({}));
        assert_eq!(capture_target(&registry, &live, None, stash("x")), None);
    }

    #[test]
    fn capture_target_needs_the_stash_token_for_a_provisional_org_match() {
        let registry = provisional_registry();
        let live = ids(Some("acct-1"), "org-1");
        assert_eq!(
            capture_target(&registry, &live, Some("tok"), stash("tok")),
            Some(0)
        );
        assert_eq!(
            capture_target(&registry, &live, Some("other"), stash("tok")),
            None
        );
        assert_eq!(capture_target(&registry, &live, None, stash("tok")), None);
    }

    #[test]
    fn capture_target_rejects_two_provisionals_in_one_org() {
        let registry = two_provisionals();
        let live = ids(None, "org-1");
        let both = |_: &str| Some("tok".to_string());
        assert_eq!(capture_target(&registry, &live, Some("tok"), both), None);
    }

    #[test]
    fn earliest_record_falls_back_to_now_when_empty() {
        let before = Utc::now();
        assert!(earliest_record(&[]) >= before);
    }

    fn rekey_home(tag: &str) -> (PathBuf, paths::TestHome) {
        let dir = std::env::temp_dir().join(format!(
            "tku-rekey-test-{tag}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(dir.join("cache")).unwrap();
        let home = paths::TestHome::set(&dir);
        (dir, home)
    }

    fn org_registry() -> Registry {
        let mut registry = Registry::default();
        register_default(
            &mut registry,
            Utc::now(),
            info(),
            "org-1".to_string(),
            "org-1".to_string(),
            None,
        );
        registry
    }

    fn transcript_root(id: &str) -> PathBuf {
        paths::transcripts_dir(TOOL_CLAUDE).unwrap().join(id)
    }

    fn seed_transcripts(id: &str) -> PathBuf {
        let projects = paths::spawn_transcripts_dir(TOOL_CLAUDE, id).unwrap();
        fs::create_dir_all(projects.join("p")).unwrap();
        let file = projects.join("p").join("s.jsonl");
        fs::write(&file, id).unwrap();
        file
    }

    fn store_with(file: &Path) -> crate::storage::bitcode_store::BitcodeStorage {
        use crate::storage::Storage;
        let mut store = crate::storage::bitcode_store::BitcodeStorage::new();
        let record = UsageRecord {
            provider: crate::types::Provider::Claude,
            session_id: "s".into(),
            timestamp: Utc::now(),
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
            account_uuid: Some("org-1".into()),
        };
        store.insert("claude", file, 0, 0, vec![record]);
        store
    }

    fn moved(file: &Path, old: &str, new: &str) -> PathBuf {
        let rest = file.strip_prefix(transcript_root(old)).unwrap();
        transcript_root(new).join(rest)
    }

    #[test]
    fn rekey_profile_cache_drops_old_when_target_exists() {
        let (dir, home) = rekey_home("profile");
        let old = paths::profile_cache_file("org-1").unwrap();
        let new = paths::profile_cache_file("acct-1@org-1").unwrap();

        fs::write(&old, "old").unwrap();
        rekey_profile_cache("org-1", "acct-1@org-1").unwrap();
        assert!(!old.exists());
        assert_eq!(fs::read_to_string(&new).unwrap(), "old");

        fs::write(&old, "stale").unwrap();
        fs::write(&new, "fresh").unwrap();
        rekey_profile_cache("org-1", "acct-1@org-1").unwrap();
        assert!(!old.exists());
        assert_eq!(fs::read_to_string(&new).unwrap(), "fresh");

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_transcripts_renames() {
        let (dir, home) = rekey_home("transcripts");
        rekey_transcripts("default", "org-1", "acct-1@org-1").unwrap();

        let file = seed_transcripts("org-1");
        rekey_transcripts("default", "org-1", "acct-1@org-1").unwrap();
        assert!(!transcript_root("org-1").exists());
        assert!(moved(&file, "org-1", "acct-1@org-1").exists());
        let lock = crate::spawn::lock_path(&paths::spawn_dir(TOOL_CLAUDE).unwrap(), "default");
        assert!(!lock.exists());

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_transcripts_target_exists_leaves_both() {
        let (dir, home) = rekey_home("transcripts-both");
        let old_file = seed_transcripts("org-1");
        let new_file = seed_transcripts("acct-1@org-1");

        rekey_transcripts("default", "org-1", "acct-1@org-1").unwrap();
        assert_eq!(fs::read_to_string(&old_file).unwrap(), "org-1");
        assert_eq!(fs::read_to_string(&new_file).unwrap(), "acct-1@org-1");

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_transcripts_skips_while_lock_held() {
        use crate::storage::Storage;
        let (dir, home) = rekey_home("transcripts-locked");
        let file = seed_transcripts("org-1");
        let mut store = store_with(&file);
        let mut registry = org_registry();

        let parent = crate::spawn::private_spawn_dir().unwrap();
        let lock = crate::spawn::SpawnLock::acquire(&crate::spawn::lock_path(&parent, "default"))
            .unwrap()
            .unwrap();
        rekey(&mut registry, &mut store, "org-1", "acct-1@org-1");
        drop(lock);

        assert!(file.exists());
        assert!(!transcript_root("acct-1@org-1").exists());
        assert_eq!(registry.accounts[0].id, "acct-1@org-1");
        assert!(store.is_cached("claude", &file, 0, 0));
        assert!(!store.is_cached("claude", &moved(&file, "org-1", "acct-1@org-1"), 0, 0));

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rekey_remaps_paths_only_after_rename() {
        use crate::storage::Storage;
        let (dir, home) = rekey_home("remap");

        let file = seed_transcripts("org-1");
        let mut store = store_with(&file);
        let mut registry = org_registry();
        rekey(&mut registry, &mut store, "org-1", "acct-1@org-1");
        let target = moved(&file, "org-1", "acct-1@org-1");
        assert!(target.exists());
        assert!(store.is_cached("claude", &target, 0, 0));
        assert!(!store.is_cached("claude", &file, 0, 0));

        fs::remove_dir_all(transcript_root("acct-1@org-1")).unwrap();
        let file = seed_transcripts("org-1");
        seed_transcripts("acct-1@org-1");
        let mut store = store_with(&file);
        let mut registry = org_registry();
        rekey(&mut registry, &mut store, "org-1", "acct-1@org-1");
        assert!(file.exists());
        assert!(store.is_cached("claude", &file, 0, 0));
        assert!(!store.is_cached("claude", &moved(&file, "org-1", "acct-1@org-1"), 0, 0));

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn rekey_continues_after_a_failing_step() {
        use crate::storage::Storage;
        use std::os::unix::fs::PermissionsExt;
        let (dir, home) = rekey_home("failing");
        let file = seed_transcripts("org-1");
        let mut store = store_with(&file);
        let mut registry = org_registry();

        let snapshots = paths::subscription_snapshot_file(TOOL_CLAUDE).unwrap();
        let data = r#"{"version":2,"accounts":{"org-1":{"snapshots":[]}}}"#;
        fs::write(&snapshots, data).unwrap();
        let cache = paths::cache_dir().unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o500)).unwrap();

        rekey(&mut registry, &mut store, "org-1", "acct-1@org-1");
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(fs::read_to_string(&snapshots).unwrap(), data);
        assert_eq!(registry.accounts[0].id, "acct-1@org-1");
        assert_eq!(registry.switch_log[0].id, "acct-1@org-1");
        assert!(store.is_cached("claude", &moved(&file, "org-1", "acct-1@org-1"), 0, 0));
        let records = store.drain_all();
        assert_eq!(records[0].account_uuid.as_deref(), Some("acct-1@org-1"));

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    const AGG_ORG: &str = "tku-agg:s:2026-01-01:opus:org-1:false";
    const AGG_ACCT: &str = "tku-agg:s:2026-01-01:opus:acct-1@org-1:false";

    fn migration_record(account: &str, message_id: &str, timestamp: DateTime<Utc>) -> UsageRecord {
        UsageRecord {
            provider: crate::types::Provider::Claude,
            session_id: "s".into(),
            timestamp,
            project: "p".into(),
            model: "opus".into(),
            message_id: message_id.into(),
            request_id: "r".into(),
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: false,
            account_uuid: Some(account.into()),
        }
    }

    fn write_registry(version: u32) {
        let path = paths::registry_file(TOOL_CLAUDE).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let json = format!(
            r#"{{
                "version": {version},
                "accounts": [
                    {{
                        "name": "work",
                        "org_uuid": "org-1",
                        "added_at": "2025-01-01T00:00:00Z",
                        "last_used_at": "2025-01-02T00:00:00Z",
                        "oauth_account": {{ "accountUuid": "acct-1", "organizationUuid": "org-1" }}
                    }},
                    {{
                        "name": "side",
                        "org_uuid": "org-2",
                        "added_at": "2025-01-01T00:00:00Z",
                        "last_used_at": "2025-01-02T00:00:00Z"
                    }}
                ],
                "switch_log": [
                    {{ "at": "2025-01-01T00:00:00Z", "org_uuid": "org-1", "name": "work", "source": "bootstrap" }},
                    {{ "at": "2025-01-02T00:00:00Z", "org_uuid": "org-2", "name": "side", "source": "implicit" }}
                ]
            }}"#
        );
        fs::write(path, json).unwrap();
    }

    /// Org-keyed state for `work` (org-1, blob acct-1) and `side` (org-2, no
    /// blob). Returns the transcript file that is also a record cache key.
    fn seed_migration(version: u32) -> PathBuf {
        write_registry(version);
        let file = paths::spawn_transcripts_dir(TOOL_CLAUDE, "org-1")
            .unwrap()
            .join("x.jsonl");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{}").unwrap();

        let old = DateTime::parse_from_rfc3339("2026-01-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut store = crate::storage::default_storage();
        store.insert(
            "claude",
            &file,
            0,
            0,
            vec![
                migration_record("org-1", "m1", Utc::now()),
                migration_record("org-1", AGG_ORG, old),
            ],
        );
        store.insert(
            "claude",
            Path::new("/proj/b.jsonl"),
            0,
            0,
            vec![migration_record("org-2", "m2", Utc::now())],
        );
        store.flush();

        let snapshots = paths::subscription_snapshot_file(TOOL_CLAUDE).unwrap();
        let data =
            r#"{"version":2,"accounts":{"org-1":{"snapshots":[]},"org-2":{"snapshots":[]}}}"#;
        fs::write(snapshots, data).unwrap();
        fs::write(paths::profile_cache_file("org-1").unwrap(), "p1").unwrap();
        fs::write(paths::profile_cache_file("org-2").unwrap(), "p2").unwrap();
        file
    }

    fn registry_ids() -> (u32, Vec<String>, Vec<String>) {
        let registry = load_registry(TOOL_CLAUDE);
        let accounts = registry.accounts.iter().map(|a| a.id.clone()).collect();
        let log = registry.switch_log.iter().map(|e| e.id.clone()).collect();
        (registry.version, accounts, log)
    }

    fn stored_records() -> Vec<(String, String)> {
        let mut store = crate::storage::default_storage();
        // The bitcode store loads a provider file on first access only.
        store.is_cached("claude", Path::new("/proj/b.jsonl"), 0, 0);
        let mut out: Vec<(String, String)> = store
            .drain_all()
            .into_iter()
            .map(|r| (r.message_id, r.account_uuid.unwrap_or_default()))
            .collect();
        out.sort();
        out
    }

    fn snapshot_keys() -> Vec<String> {
        let path = paths::subscription_snapshot_file(TOOL_CLAUDE).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let mut keys: Vec<String> = json["accounts"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    fn assert_org_keyed(file: &Path) {
        assert!(file.exists());
        assert!(!transcript_root("acct-1@org-1").exists());
        assert_eq!(
            stored_records(),
            [
                ("m1".to_string(), "org-1".to_string()),
                ("m2".to_string(), "org-2".to_string()),
                (AGG_ORG.to_string(), "org-1".to_string()),
            ]
        );
        assert_eq!(snapshot_keys(), ["org-1", "org-2"]);
        assert!(paths::profile_cache_file("org-1").unwrap().exists());
    }

    #[test]
    fn migrates_v1_registry_end_to_end() {
        let (dir, home) = rekey_home("migrate");
        let file = seed_migration(1);

        migrate_to_account_ids();

        assert_eq!(registry_ids(), migrated_ids());
        let registry = load_registry(TOOL_CLAUDE);
        assert_eq!(registry.accounts[0].org_uuid, "org-1");
        assert!(registry.accounts[1].is_provisional());

        let target = moved(&file, "org-1", "acct-1@org-1");
        assert!(!transcript_root("org-1").exists());
        assert!(target.exists());
        let mut store = crate::storage::default_storage();
        assert!(store.is_cached("claude", &target, 0, 0));
        assert!(!store.is_cached("claude", &file, 0, 0));
        assert!(store.is_cached("claude", Path::new("/proj/b.jsonl"), 0, 0));
        drop(store);
        assert_eq!(stored_records(), migrated_records());

        assert_eq!(snapshot_keys(), ["acct-1@org-1", "org-2"]);
        assert!(!paths::profile_cache_file("org-1").unwrap().exists());
        let profile = paths::profile_cache_file("acct-1@org-1").unwrap();
        assert_eq!(fs::read_to_string(profile).unwrap(), "p1");
        let profile = paths::profile_cache_file("org-2").unwrap();
        assert_eq!(fs::read_to_string(profile).unwrap(), "p2");

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_keeps_one_person_in_two_orgs_separate() {
        let (dir, home) = rekey_home("migrate-two-orgs");
        let path = paths::registry_file(TOOL_CLAUDE).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let account = |name: &str, org: &str| {
            serde_json::json!({
                "name": name,
                "org_uuid": org,
                "added_at": "2025-01-01T00:00:00Z",
                "last_used_at": "2025-01-02T00:00:00Z",
                "oauth_account": { "accountUuid": "acct-1", "organizationUuid": org },
            })
        };
        let switch = |at: &str, name: &str, org: &str| serde_json::json!({ "at": at, "org_uuid": org, "name": name, "source": "explicit" });
        let registry = serde_json::json!({
            "version": 1,
            "accounts": [account("personal", "org-1"), account("team", "org-2")],
            "switch_log": [
                switch("2025-01-01T00:00:00Z", "personal", "org-1"),
                switch("2025-01-02T00:00:00Z", "team", "org-2"),
            ],
        });
        fs::write(&path, registry.to_string()).unwrap();
        let mut store = crate::storage::default_storage();
        store.insert(
            "claude",
            Path::new("/proj/a.jsonl"),
            0,
            0,
            vec![migration_record("org-1", "m1", Utc::now())],
        );
        store.insert(
            "claude",
            Path::new("/proj/b.jsonl"),
            0,
            0,
            vec![migration_record("org-2", "m2", Utc::now())],
        );
        store.flush();

        migrate_to_account_ids();

        let ids = vec!["acct-1@org-1".to_string(), "acct-1@org-2".to_string()];
        assert_eq!(registry_ids(), (2, ids.clone(), ids));
        assert_eq!(
            stored_records(),
            [
                ("m1".to_string(), "acct-1@org-1".to_string()),
                ("m2".to_string(), "acct-1@org-2".to_string()),
            ]
        );

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_is_idempotent_on_rerun() {
        let (dir, home) = rekey_home("migrate-rerun");
        seed_migration(1);
        let registry_file = paths::registry_file(TOOL_CLAUDE).unwrap();

        migrate_to_account_ids();
        let registry = fs::read_to_string(&registry_file).unwrap();
        let records = stored_records();
        let snapshots = snapshot_keys();

        migrate_to_account_ids();
        assert_eq!(fs::read_to_string(&registry_file).unwrap(), registry);
        assert_eq!(stored_records(), records);
        assert_eq!(snapshot_keys(), snapshots);
        assert!(transcript_root("acct-1@org-1").exists());
        assert!(!transcript_root("org-1").exists());

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_2_registry_is_skipped() {
        let (dir, home) = rekey_home("migrate-v2");
        migrate_to_account_ids();
        assert!(!paths::registry_file(TOOL_CLAUDE).unwrap().exists());

        let file = seed_migration(2);
        migrate_to_account_ids();

        let (version, accounts, _) = registry_ids();
        assert_eq!(
            (version, accounts),
            (2, vec!["org-1".into(), "org-2".into()])
        );
        assert_org_keyed(&file);

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_skips_while_lock_held() {
        let (dir, home) = rekey_home("migrate-locked");
        let file = seed_migration(1);

        let parent = crate::spawn::private_spawn_dir().unwrap();
        let lock = crate::spawn::SpawnLock::acquire(&parent.join(".migrate.lock"))
            .unwrap()
            .unwrap();
        migrate_with_wait(std::time::Duration::from_millis(200));
        drop(lock);

        let (version, accounts, _) = registry_ids();
        assert_eq!(
            (version, accounts),
            (1, vec!["org-1".into(), "org-2".into()])
        );
        assert_org_keyed(&file);

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_waits_for_a_held_lock() {
        let (dir, home) = rekey_home("migrate-wait");
        seed_migration(1);

        let parent = crate::spawn::private_spawn_dir().unwrap();
        let lock = crate::spawn::SpawnLock::acquire(&parent.join(".migrate.lock"))
            .unwrap()
            .unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            drop(lock);
        });
        migrate_to_account_ids();
        holder.join().unwrap();

        assert_eq!(registry_ids(), migrated_ids());
        assert_eq!(stored_records(), migrated_records());

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    fn migrated_ids() -> (u32, Vec<String>, Vec<String>) {
        let ids = vec!["acct-1@org-1".to_string(), "org-2".to_string()];
        (2, ids.clone(), ids)
    }

    fn migrated_records() -> Vec<(String, String)> {
        vec![
            ("m1".to_string(), "acct-1@org-1".to_string()),
            ("m2".to_string(), "org-2".to_string()),
            (AGG_ACCT.to_string(), "acct-1@org-1".to_string()),
        ]
    }

    #[test]
    fn registry_saved_at_version_2_when_a_non_store_step_fails() {
        let (dir, home) = rekey_home("migrate-step-fails");
        let file = seed_migration(1);
        let snapshots = paths::subscription_snapshot_file(TOOL_CLAUDE).unwrap();
        fs::remove_file(&snapshots).unwrap();
        fs::create_dir(&snapshots).unwrap();

        migrate_to_account_ids();

        assert_eq!(registry_ids(), migrated_ids());
        assert_eq!(stored_records(), migrated_records());
        assert!(snapshots.is_dir());
        assert!(paths::profile_cache_file("acct-1@org-1").unwrap().exists());
        assert!(moved(&file, "org-1", "acct-1@org-1").exists());

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(all(unix, not(feature = "sqlite")))]
    #[test]
    fn registry_stays_v1_when_the_cache_flush_fails() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, home) = rekey_home("migrate-flush-fails");
        let file = seed_migration(1);
        let target = moved(&file, "org-1", "acct-1@org-1");

        let cache = paths::cache_dir().unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o500)).unwrap();
        migrate_to_account_ids();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();

        let (version, accounts, _) = registry_ids();
        assert_eq!(
            (version, accounts),
            (1, vec!["org-1".into(), "org-2".into()])
        );
        assert!(!transcript_root("org-1").exists());
        assert!(target.exists());

        migrate_to_account_ids();

        assert_eq!(registry_ids(), migrated_ids());
        assert_eq!(stored_records(), migrated_records());
        let mut store = crate::storage::default_storage();
        assert!(store.is_cached("claude", &target, 0, 0));
        assert!(!store.is_cached("claude", &file, 0, 0));
        drop(store);
        assert_eq!(snapshot_keys(), ["acct-1@org-1", "org-2"]);

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_at_with_interleaved_same_org_switches() {
        use test_support::{same_org_registry, ts};
        let registry = same_org_registry();
        let owner = |hour| registry.account_at(ts(hour)).map(|e| e.id.as_str());
        assert_eq!(owner(-1), None);
        assert_eq!(owner(0), Some("acct-a@org-1"));
        assert_eq!(owner(1), Some("acct-b@org-1"));
        assert_eq!(owner(2), Some("acct-a@org-1"));
        assert_eq!(owner(9), Some("acct-a@org-1"));
    }

    #[test]
    fn owner_of_unknown_tag_falls_back_to_switch_log() {
        use test_support::{same_org_registry, ts};
        let registry = same_org_registry();
        assert_eq!(registry.owner_of(Some("acct-b@org-1"), ts(0)), Some("bob"));
        assert_eq!(registry.owner_of(Some("acct-gone"), ts(1)), Some("bob"));
        assert_eq!(registry.owner_of(None, ts(2)), Some("alice"));
        assert_eq!(registry.owner_of(Some("acct-gone"), ts(-1)), None);
    }

    fn creds_bytes(token: &str) -> Vec<u8> {
        serde_json::json!({ "claudeAiOauth": { "accessToken": token } })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn add_registers_same_org_identities_as_separate_accounts() {
        let (dir, home) = rekey_home("add-same-org");
        for (name, account_uuid, token) in
            [("alice", "acct-a", "tok-a"), ("bob", "acct-b", "tok-b")]
        {
            let data = creds_bytes(token);
            let info = parse_creds_info(&data).unwrap();
            add_identity(
                name,
                &data,
                info,
                token,
                identity(account_uuid, "org-1"),
                stashed_access_token,
            )
            .unwrap();
        }

        let registry = load_registry(TOOL_CLAUDE);
        let ids: Vec<(&str, &str)> = registry
            .accounts
            .iter()
            .map(|a| (a.name.as_str(), a.id.as_str()))
            .collect();
        assert_eq!(ids, [("alice", "acct-a@org-1"), ("bob", "acct-b@org-1")]);
        assert!(registry.accounts.iter().all(|a| a.org_uuid == "org-1"));
        assert_eq!(stashed_access_token("alice").as_deref(), Some("tok-a"));
        assert_eq!(stashed_access_token("bob").as_deref(), Some("tok-b"));
        let switches: Vec<&str> = registry.switch_log.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(switches, ["acct-a@org-1", "acct-b@org-1"]);

        drop(home);
        let _ = fs::remove_dir_all(&dir);
    }
}
