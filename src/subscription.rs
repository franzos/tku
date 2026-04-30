use std::collections::BTreeMap;
use std::fs;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, ContentArrangement, Table};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::accounts::redact;
use crate::atomic_write::atomic_write;
use crate::cost::PricingMap;
use crate::exchange::ExchangeRate;
use crate::paths;
use crate::types::{Provider, UsageRecord};

const USAGE_API_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_API_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";
const PROFILE_CACHE_TTL_HOURS: i64 = 24;

/// Calibration thresholds — we fetch from the API when the estimated %
/// crosses one of these, to reconcile the local estimate with truth.
const CALIBRATION_THRESHOLDS: &[f64] = &[
    2.5, 5.0, 10.0, 30.0, 50.0, 70.0, 90.0, 95.0, 96.0, 97.0, 98.0, 99.0, 100.0,
];

// --- API response ---

#[derive(Debug, Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
    seven_day_sonnet: Option<UsageWindow>,
    seven_day_opus: Option<UsageWindow>,
    #[allow(dead_code)]
    seven_day_cowork: Option<UsageWindow>,
    #[allow(dead_code)]
    seven_day_oauth_apps: Option<UsageWindow>,
    extra_usage: Option<ExtraUsage>,
}

#[derive(Debug, Deserialize)]
struct UsageWindow {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtraUsage {
    is_enabled: bool,
    monthly_limit: f64,
    used_credits: f64,
}

// --- Profile API response (live plan detection) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileResponse {
    #[serde(default)]
    account: Option<ProfileAccount>,
    #[serde(default)]
    organization: Option<ProfileOrganization>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileAccount {
    #[serde(default)]
    has_claude_max: Option<bool>,
    #[serde(default)]
    has_claude_pro: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileOrganization {
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
    #[serde(default)]
    organization_type: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedProfile {
    captured_at: DateTime<Utc>,
    profile: ProfileResponse,
}

// --- OAuth credentials ---

#[derive(Debug, Clone, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OAuthCredentials>,
    #[serde(rename = "organizationUuid")]
    organization_uuid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OAuthCredentials {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: u64,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

// --- Snapshot persistence ---

/// Current on-disk format (v2): a version tag plus a map of
/// `organization_uuid → snapshots`. This lets us keep separate cycle
/// histories per account when the user swaps credentials.
#[derive(Debug, Serialize, Deserialize)]
struct OnDiskStore {
    version: u32,
    #[serde(default)]
    accounts: BTreeMap<String, AccountSnapshots>,
}

impl OnDiskStore {
    fn default_v2() -> Self {
        Self {
            version: 2,
            accounts: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AccountSnapshots {
    #[serde(default)]
    snapshots: Vec<CycleSnapshot>,
}

/// Per-account view that the rest of the module operates on.
/// Loaded from / saved into a single slot in the on-disk `OnDiskStore`.
#[derive(Debug, Default)]
struct SnapshotStore {
    org_uuid: String,
    snapshots: Vec<CycleSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CycleSnapshot {
    cycle_end: DateTime<Utc>,
    utilization: f64,
    captured_at: DateTime<Utc>,
    /// Local $ cost at the time of the last API calibration.
    /// Used to estimate current % between API calls.
    #[serde(default)]
    cost_at_calibration: Option<f64>,
    /// Plan in effect when this snapshot was captured. `None` for legacy
    /// snapshots written before plan-tagging existed.
    #[serde(default)]
    plan: Option<Plan>,
}

/// Whether the current % value came from the API or was estimated locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageSource {
    Live,
    Estimated,
    Cached,
}

// --- Public entry point ---

pub fn run(
    exchange: &ExchangeRate,
    records: &[UsageRecord],
    pricing: &dyn PricingMap,
    offline: bool,
    live: bool,
    plan: bool,
    account: Option<&str>,
) -> Result<()> {
    // When `--account` is specified, load that account's stashed credentials
    // and the org UUID it was registered under. Otherwise fall back to the
    // currently-active credentials in `~/.claude/.credentials.json`. The
    // subscription/usage API is per-account: we have to use the matching
    // OAuth token, or we'd be reporting the wrong account's window.
    let registry = crate::accounts::load_registry("claude");

    // Plan mode with multiple registered accounts is ambiguous — the
    // recommendation is per-account, so we refuse to guess.
    if plan && account.is_none() && registry.accounts.len() > 1 {
        eprintln!(
            "Multiple Claude accounts registered. Specify --account <name>, or use `tku sub --all` for an overview."
        );
        std::process::exit(1);
    }

    let (creds, requested_org) = if let Some(name) = account {
        let acct = registry.find_by_name(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Account '{name}' not found. Run `tku account list` to see available accounts."
            )
        })?;
        // If the requested account is also the currently active one, prefer
        // the live credentials file. Claude Code refreshes the access token
        // in `~/.claude/.credentials.json` on each run but never writes back
        // to the stashed copy — so the stash for the active account drifts
        // stale and would otherwise fail the expiry check below.
        //
        // Match by org_uuid when present; fall back to the latest switch-log
        // entry, since Claude Code drops `organizationUuid` from creds after
        // a token refresh and we'd otherwise misidentify the active account.
        let live_org = crate::accounts::current_claude_org_uuid()
            .or_else(|| registry.latest_switch().map(|s| s.org_uuid.clone()));
        let use_live = live_org.as_deref() == Some(acct.org_uuid.as_str());
        let creds = if use_live {
            load_credentials().with_context(|| {
                format!("Cannot load live credentials while account '{name}' is active")
            })?
        } else {
            let path = crate::accounts::stashed_creds_path("claude", name)
                .ok_or_else(|| anyhow::anyhow!("cannot determine stash path"))?;
            if !path.exists() {
                bail!(
                    "Stashed credentials missing for '{name}': {}. Re-add with `tku account add`.",
                    redact(&path)
                );
            }
            let data = fs::read_to_string(&path)
                .with_context(|| format!("Cannot read {}", redact(&path)))?;
            serde_json::from_str(&data).context("Failed to parse stashed credentials")?
        };
        (creds, Some(acct.org_uuid.clone()))
    } else {
        match load_credentials() {
            Ok(c) => (c, None),
            Err(_) => {
                eprintln!("Claude Code credentials not found (~/.claude/.credentials.json).");
                eprintln!("Run Claude Code at least once to create them.");
                eprintln!();
                eprintln!("The subscription command currently only supports Claude Max/Pro.");
                std::process::exit(1);
            }
        }
    };

    let oauth = match creds.claude_ai_oauth {
        Some(o) => o,
        None => {
            eprintln!("No Claude OAuth token found in credentials file.");
            eprintln!("Sign in to Claude Code to generate an OAuth token.");
            std::process::exit(1);
        }
    };

    // Resolve the org UUID we should key snapshots under. With `--account`,
    // prefer the registry's registered UUID — stashed creds may have been
    // refreshed since registration and dropped the field. Without `--account`,
    // use the live creds, falling back to the registry's latest switch entry
    // to keep snapshots from getting stranded under an "unknown" key.
    let org_uuid = requested_org
        .or(creds.organization_uuid)
        .or_else(|| registry.latest_switch().map(|s| s.org_uuid.clone()))
        .unwrap_or_else(|| "unknown".to_string());

    let now_ms = Utc::now().timestamp_millis() as u64;
    if now_ms > oauth.expires_at {
        if account.is_some() {
            eprintln!(
                "Stashed OAuth token for this account has expired. Switch to it with `tku account use <name>` and run Claude Code once to refresh."
            );
        } else {
            eprintln!("Claude OAuth token expired. Run Claude Code to refresh your session.");
        }
        std::process::exit(1);
    }

    // Filter records to the right account. Without a filter we keep all
    // Claude records (single-account behavior). With `--account`, restrict
    // by the recorded `account_uuid`, falling back to the timestamp-based
    // switch log for legacy records that pre-date scan-time tagging.
    let claude_records: Vec<&UsageRecord> = records
        .iter()
        .filter(|r| r.provider == Provider::Claude)
        .filter(|r| match account {
            None => true,
            Some(name) => match r.account_uuid.as_deref() {
                Some(uuid) => registry
                    .find_by_org(uuid)
                    .map(|a| a.name == *name)
                    .unwrap_or(false),
                None => registry
                    .account_at(r.timestamp)
                    .map(|e| e.name == *name)
                    .unwrap_or(false),
            },
        })
        .collect();

    let mut store = load_snapshots(&org_uuid);

    // Try to determine cycle boundaries from existing snapshots first
    let cached_cycle = current_cycle_from_snapshots(&store);

    // Decide whether we need an API call
    let needs_fetch = if offline {
        false
    } else if live {
        true
    } else if let Some((cycle_start, cycle_end)) = cached_cycle {
        let target = round_to_minute(cycle_end);
        let snapshot = store
            .snapshots
            .iter()
            .find(|s| round_to_minute(s.cycle_end) == target);
        match snapshot {
            None => true,
            Some(snap) => {
                // Below 2.5%: always fetch (not enough data for reliable ratio)
                if snap.utilization < 2.5 {
                    true
                } else if let Some(cal_cost) = snap.cost_at_calibration {
                    // Estimate current % and check if we crossed a threshold
                    let cost_now = cost_in_range(&claude_records, cycle_start, Utc::now(), pricing);
                    let cost_per_pct = cal_cost / snap.utilization;
                    let estimated = if cost_per_pct > 0.0 {
                        cost_now.unwrap_or(0.0) / cost_per_pct
                    } else {
                        snap.utilization
                    };
                    should_calibrate(snap.utilization, estimated)
                } else {
                    // Legacy snapshot without cost — calibrate once to get the ratio
                    true
                }
            }
        }
    } else {
        // No snapshot at all — must fetch
        true
    };

    let usage = if needs_fetch {
        match fetch_usage(&oauth.access_token) {
            Ok(u) => Some(u),
            Err(_) => {
                // Opaque message — don't interpolate the underlying error
                // chain, which can include the request URL and other details
                // we'd rather not echo to stderr by default. Detailed
                // diagnostics belong behind a future `--verbose` flag.
                eprintln!("usage: failed to fetch usage (check credentials / network)");
                None
            }
        }
    } else {
        None
    };

    let seven_day = usage.as_ref().and_then(|u| u.seven_day.as_ref());

    let resets_at = seven_day
        .and_then(|w| w.resets_at.as_ref())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok());

    let resets_at = match resets_at {
        Some(r) => r,
        None => {
            if let Some((_, end)) = cached_cycle {
                end
            } else {
                bail!(
                    "Cannot determine billing cycle. Run with network access to fetch current usage."
                );
            }
        }
    };

    let cycle_start = resets_at - Duration::days(7);
    let current_cost = cost_in_range(&claude_records, cycle_start, Utc::now(), pricing);

    // Live plan: profile API → cached profile → OAuth claims. Falling back
    // silently keeps offline runs working; a stale-after-switch state is
    // bounded by the 24h profile cache.
    let (live_plan, profile) = resolve_live_plan(&oauth.access_token, &org_uuid, offline);
    let oauth_plan = detect_plan(&oauth);
    let resolved_plan = live_plan.or(oauth_plan);

    // Save snapshot for current cycle when we got live data
    if let Some(w) = &seven_day {
        save_snapshot(
            resets_at,
            w.utilization,
            current_cost,
            resolved_plan,
            &mut store,
        );
    }

    if plan {
        return run_plan_mode(&oauth, &store, resets_at, exchange, resolved_plan, &usage);
    }

    let cycles = compute_cycles(resets_at, 4);

    // Resolve current week's utilization + source
    let (current_pct, current_source) = resolve_current_usage(
        usage.as_ref(),
        &store,
        resets_at,
        &claude_records,
        pricing,
        current_cost,
    );

    // Header. Prefer the live profile's tier label; fall back to OAuth
    // claims (which can be stale after a plan switch on anthropic.com).
    let sub_type = oauth.subscription_type.as_deref().unwrap_or("unknown");
    let tier = oauth.rate_limit_tier.as_deref().unwrap_or("");
    let tier_label = profile
        .as_ref()
        .and_then(format_tier_from_profile)
        .unwrap_or_else(|| format_tier(sub_type, tier));

    if let Some(pct) = current_pct {
        let prefix = match current_source {
            UsageSource::Estimated => "~",
            _ => "",
        };
        let reset_local = resets_at.with_timezone(&chrono::Local);
        eprintln!(
            "{} — {}{:.0}% used, resets {}",
            tier_label,
            prefix,
            pct,
            reset_local.format("%b %-d, %-I:%M%P")
        );
    } else {
        eprintln!("{} (offline — showing cached data)", tier_label);
    }

    // Pace projection bar
    if let Some(pct) = current_pct {
        if pct > 0.0 {
            let now = Utc::now();
            let elapsed = now.signed_duration_since(cycle_start);
            let elapsed_h = elapsed.num_minutes() as f64 / 60.0;
            let rate_per_h = pct / elapsed_h;
            let cycle_remaining_h =
                resets_at.signed_duration_since(now).num_minutes() as f64 / 60.0;
            let projected = (pct + rate_per_h * cycle_remaining_h).min(200.0);

            const BAR_W: usize = 30;
            let used_w = ((pct / 100.0) * BAR_W as f64).round() as usize;
            let proj_w = if projected > pct {
                (((projected - pct) / 100.0) * BAR_W as f64).round() as usize
            } else {
                0
            };
            let used_w = used_w.min(BAR_W);
            let proj_w = proj_w.min(BAR_W - used_w);
            let free_w = BAR_W - used_w - proj_w;

            let bar = format!(
                "{}{}{}",
                "█".repeat(used_w),
                "▒".repeat(proj_w),
                "░".repeat(free_w),
            );

            if pct >= 100.0 {
                eprintln!("{} ▸ at capacity", bar);
            } else if projected >= 100.0 {
                eprintln!(
                    "{} ▸ hits 100% in {}",
                    bar,
                    format_duration_short((100.0 - pct) / rate_per_h)
                );
            } else {
                eprintln!(
                    "{} ▸ ~{:.0}% at reset, {} left",
                    bar,
                    projected,
                    format_duration_short(cycle_remaining_h)
                );
            }
        }
    }

    eprintln!();

    // Table
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        Cell::new("Period"),
        Cell::new("Usage"),
        Cell::new("Cost"),
        Cell::new("$/1%"),
        Cell::new("Overage"),
    ]);

    for (i, (start, end)) in cycles.iter().enumerate() {
        let is_current = i == cycles.len() - 1;
        let period_label = format!("{} → {}", start.format("%b %-d"), end.format("%b %-d"));

        let (usage_pct, source) = if is_current {
            (current_pct, current_source)
        } else {
            match find_snapshot(&store, *end) {
                Some(pct) => (Some(pct), UsageSource::Cached),
                None => (None, UsageSource::Cached),
            }
        };

        let usage_str = match (usage_pct, source) {
            (Some(pct), UsageSource::Estimated) => format!("~{:.0}%", pct),
            (Some(pct), _) => format!("{:.0}%", pct),
            (None, _) => "—".to_string(),
        };

        let cost = cost_in_range(&claude_records, *start, *end, pricing);
        let cost_str = exchange.format_cost(cost);

        let cost_per_pct = match (usage_pct, cost) {
            (Some(pct), Some(c)) if pct > 0.0 => exchange.format_cost(Some(c / pct)),
            _ => "—".to_string(),
        };

        let overage_str = if is_current {
            format_overage(
                usage.as_ref().and_then(|u| u.extra_usage.as_ref()),
                exchange,
            )
        } else {
            "—".to_string()
        };

        table.add_row(vec![
            Cell::new(&period_label),
            Cell::new(&usage_str),
            Cell::new(&cost_str),
            Cell::new(&cost_per_pct),
            Cell::new(&overage_str),
        ]);

        // Sub-rows for current period (only when we have live API data)
        if is_current {
            if let Some(ref u) = usage {
                if let Some(ref w) = u.five_hour {
                    table.add_row(vec![
                        Cell::new("  └─ 5h window"),
                        Cell::new(format!("{:.0}%", w.utilization)),
                        Cell::new(""),
                        Cell::new(""),
                        Cell::new(""),
                    ]);
                }
                if let Some(ref w) = u.seven_day_sonnet {
                    if w.utilization > 0.0 {
                        table.add_row(vec![
                            Cell::new("  └─ Sonnet"),
                            Cell::new(format!("{:.0}%", w.utilization)),
                            Cell::new(""),
                            Cell::new(""),
                            Cell::new(""),
                        ]);
                    }
                }
                if let Some(ref w) = u.seven_day_opus {
                    if w.utilization > 0.0 {
                        table.add_row(vec![
                            Cell::new("  └─ Opus"),
                            Cell::new(format!("{:.0}%", w.utilization)),
                            Cell::new(""),
                            Cell::new(""),
                            Cell::new(""),
                        ]);
                    }
                }
            }
        }
    }

    println!("{table}");

    Ok(())
}

// --- Calibration logic ---

/// Check if the estimated % has crossed any calibration threshold
/// above the last real (API-confirmed) utilization.
fn should_calibrate(last_real_pct: f64, estimated_pct: f64) -> bool {
    CALIBRATION_THRESHOLDS
        .iter()
        .any(|&t| last_real_pct < t && estimated_pct >= t)
}

/// Resolve the current week's utilization and its source.
fn resolve_current_usage(
    usage: Option<&UsageResponse>,
    store: &SnapshotStore,
    cycle_end: DateTime<Utc>,
    records: &[&UsageRecord],
    pricing: &dyn PricingMap,
    current_cost: Option<f64>,
) -> (Option<f64>, UsageSource) {
    // Live data takes priority
    if let Some(u) = usage {
        if let Some(ref w) = u.seven_day {
            return (Some(w.utilization), UsageSource::Live);
        }
    }

    // Try to estimate from snapshot
    let target = round_to_minute(cycle_end);
    let snapshot = store
        .snapshots
        .iter()
        .find(|s| round_to_minute(s.cycle_end) == target);
    if let Some(snap) = snapshot {
        if let Some(cal_cost) = snap.cost_at_calibration {
            if snap.utilization > 0.0 && cal_cost > 0.0 {
                let cost_per_pct = cal_cost / snap.utilization;
                let cycle_start = cycle_end - Duration::days(7);
                let cost_now = current_cost
                    .or_else(|| cost_in_range(records, cycle_start, Utc::now(), pricing));
                let estimated = cost_now.unwrap_or(0.0) / cost_per_pct;
                return (Some(estimated), UsageSource::Estimated);
            }
        }
        // Snapshot exists but can't estimate — return cached value
        return (Some(snap.utilization), UsageSource::Cached);
    }

    (None, UsageSource::Cached)
}

// --- Implementation ---

fn load_credentials() -> Result<Credentials> {
    let base = BaseDirs::new().context("Cannot determine home directory")?;
    let path = base.home_dir().join(".claude").join(".credentials.json");
    let data =
        fs::read_to_string(&path).with_context(|| format!("Cannot read {}", redact(&path)))?;
    serde_json::from_str(&data).context("Failed to parse credentials")
}

fn fetch_usage(access_token: &str) -> Result<UsageResponse> {
    let body = crate::http::agent()
        .get(USAGE_API_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", ANTHROPIC_BETA)
        .call()
        .context("Failed to call usage API")?
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .context("Failed to read usage response")?;
    serde_json::from_str(&body).context("Failed to parse usage response")
}

/// Fetch the live organization UUID from the profile API. Used by
/// `accounts::add` when the creds file doesn't carry `organizationUuid`
/// (modern Claude Code layouts) and `.claude.json` would be stale.
pub fn fetch_live_org_uuid(access_token: &str) -> Result<String> {
    let profile = fetch_profile(access_token)?;
    profile
        .organization
        .and_then(|o| o.uuid)
        .ok_or_else(|| anyhow::anyhow!("Profile API did not return organization.uuid"))
}

fn fetch_profile(access_token: &str) -> Result<ProfileResponse> {
    let body = crate::http::agent()
        .get(PROFILE_API_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", ANTHROPIC_BETA)
        .call()
        .context("Failed to call profile API")?
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .context("Failed to read profile response")?;
    serde_json::from_str(&body).context("Failed to parse profile response")
}

fn profile_cache_path(org_uuid: &str) -> Option<std::path::PathBuf> {
    paths::profile_cache_file(org_uuid)
}

fn load_cached_profile(org_uuid: &str) -> Option<ProfileResponse> {
    let path = profile_cache_path(org_uuid)?;
    let data = fs::read_to_string(&path).ok()?;
    let cached: CachedProfile = serde_json::from_str(&data).ok()?;
    let age = Utc::now().signed_duration_since(cached.captured_at);
    if age > Duration::hours(PROFILE_CACHE_TTL_HOURS) {
        return None;
    }
    Some(cached.profile)
}

fn save_cached_profile(org_uuid: &str, profile: &ProfileResponse) {
    let Some(path) = profile_cache_path(org_uuid) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("warning: failed to create profile cache dir: {e}");
            return;
        }
    }
    let cached = CachedProfile {
        captured_at: Utc::now(),
        profile: profile.clone(),
    };
    match serde_json::to_string_pretty(&cached) {
        Ok(data) => {
            if let Err(e) = atomic_write(&path, data.as_bytes(), Some(0o600)) {
                eprintln!("warning: failed to save profile cache: {e}");
            }
        }
        Err(e) => eprintln!("warning: failed to serialize profile cache: {e}"),
    }
}

/// Fetch the live plan for an account, with 24h cache. Honors `--offline`
/// (returns None without touching the network or cache freshness check).
/// Network failures fall back to a stale cache when present, then None.
fn resolve_live_plan(
    access_token: &str,
    org_uuid: &str,
    offline: bool,
) -> (Option<Plan>, Option<ProfileResponse>) {
    if offline {
        return (None, None);
    }
    if let Some(p) = load_cached_profile(org_uuid) {
        let plan = plan_from_profile(&p);
        return (plan, Some(p));
    }
    match fetch_profile(access_token) {
        Ok(p) => {
            save_cached_profile(org_uuid, &p);
            let plan = plan_from_profile(&p);
            (plan, Some(p))
        }
        Err(_) => (None, None),
    }
}

/// Map a profile response onto a `Plan`. Prefers `organization.rate_limit_tier`
/// (the source of truth for active tier), falls back to the boolean flags on
/// `account` for the Pro/Max distinction when tier is missing.
fn plan_from_profile(p: &ProfileResponse) -> Option<Plan> {
    if let Some(org) = &p.organization {
        if let Some(tier) = org.rate_limit_tier.as_deref() {
            if tier.contains("20x") {
                return Some(Plan::Max20x);
            }
            if tier.contains("5x") || tier.contains("max") {
                return Some(Plan::Max5x);
            }
            if tier.contains("pro") {
                return Some(Plan::Pro);
            }
        }
        if let Some(t) = org.organization_type.as_deref() {
            if t == "claude_max" {
                return Some(Plan::Max5x);
            }
            if t == "claude_pro" {
                return Some(Plan::Pro);
            }
        }
    }
    if let Some(acc) = &p.account {
        if acc.has_claude_max == Some(true) {
            return Some(Plan::Max5x);
        }
        if acc.has_claude_pro == Some(true) {
            return Some(Plan::Pro);
        }
    }
    None
}

/// Format a tier label from a profile response, mirroring `format_tier` for
/// OAuth claims. Used when the live profile is the source of truth.
fn format_tier_from_profile(p: &ProfileResponse) -> Option<String> {
    let plan = plan_from_profile(p)?;
    Some(plan.label().to_string())
}

fn snapshot_path() -> Option<std::path::PathBuf> {
    paths::subscription_snapshot_file("claude")
}

/// Parse the on-disk store, transparently migrating v1 (flat `snapshots`)
/// to v2 (per-org map). All legacy snapshots are attributed to `migration_org`
/// — correct for single-account users (99% case); multi-account users who
/// swapped manually before tku had account support have already-conflated
/// data that can't be retroactively split.
fn parse_on_disk(data: &str, migration_org: &str) -> OnDiskStore {
    #[derive(Deserialize)]
    struct V2Probe {
        version: u32,
        #[serde(default)]
        accounts: BTreeMap<String, AccountSnapshots>,
    }
    if let Ok(v2) = serde_json::from_str::<V2Probe>(data) {
        if v2.version >= 2 {
            return OnDiskStore {
                version: v2.version,
                accounts: v2.accounts,
            };
        }
    }

    #[derive(Deserialize)]
    struct V1 {
        snapshots: Vec<CycleSnapshot>,
    }
    if let Ok(v1) = serde_json::from_str::<V1>(data) {
        let mut accounts = BTreeMap::new();
        accounts.insert(
            migration_org.to_string(),
            AccountSnapshots {
                snapshots: v1.snapshots,
            },
        );
        return OnDiskStore {
            version: 2,
            accounts,
        };
    }

    OnDiskStore::default_v2()
}

fn load_snapshots(org_uuid: &str) -> SnapshotStore {
    let Some(path) = snapshot_path() else {
        return SnapshotStore {
            org_uuid: org_uuid.to_string(),
            snapshots: Vec::new(),
        };
    };
    let data = fs::read_to_string(&path).unwrap_or_default();
    let on_disk = parse_on_disk(&data, org_uuid);
    let snapshots = on_disk
        .accounts
        .get(org_uuid)
        .map(|a| a.snapshots.clone())
        .unwrap_or_default();
    SnapshotStore {
        org_uuid: org_uuid.to_string(),
        snapshots,
    }
}

/// In-memory snapshot mutation, factored out so the fork-on-plan-change
/// logic is unit-testable without disk I/O. Mid-cycle plan change: if an
/// existing entry for this cycle has a different tagged plan, fork — leave
/// the old entry intact and append a new one.
fn apply_snapshot(
    snapshots: &mut Vec<CycleSnapshot>,
    cycle_end: DateTime<Utc>,
    utilization: f64,
    cost: Option<f64>,
    plan: Option<Plan>,
    now: DateTime<Utc>,
) {
    let target = round_to_minute(cycle_end);
    let existing_idx = snapshots
        .iter()
        .position(|s| round_to_minute(s.cycle_end) == target);
    let should_fork = match (existing_idx, plan) {
        (Some(i), Some(new_plan)) => matches!(snapshots[i].plan, Some(old) if old != new_plan),
        _ => false,
    };

    if should_fork {
        snapshots.push(CycleSnapshot {
            cycle_end,
            utilization,
            captured_at: now,
            cost_at_calibration: cost,
            plan,
        });
    } else if let Some(i) = existing_idx {
        let existing = &mut snapshots[i];
        existing.cycle_end = cycle_end;
        existing.utilization = utilization;
        existing.captured_at = now;
        existing.cost_at_calibration = cost;
        if plan.is_some() {
            existing.plan = plan;
        }
    } else {
        snapshots.push(CycleSnapshot {
            cycle_end,
            utilization,
            captured_at: now,
            cost_at_calibration: cost,
            plan,
        });
    }

    snapshots.sort_by_key(|s| s.cycle_end);
    if snapshots.len() > 12 {
        let excess = snapshots.len() - 12;
        snapshots.drain(..excess);
    }
}

fn save_snapshot(
    cycle_end: DateTime<Utc>,
    utilization: f64,
    cost: Option<f64>,
    plan: Option<Plan>,
    store: &mut SnapshotStore,
) {
    let Some(path) = snapshot_path() else { return };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("warning: failed to create snapshot dir: {e}");
        }
    }

    apply_snapshot(
        &mut store.snapshots,
        cycle_end,
        utilization,
        cost,
        plan,
        Utc::now(),
    );

    // Re-read + merge so we don't clobber other accounts' data on write.
    let existing_data = fs::read_to_string(&path).unwrap_or_default();
    let mut on_disk = parse_on_disk(&existing_data, &store.org_uuid);
    on_disk.accounts.insert(
        store.org_uuid.clone(),
        AccountSnapshots {
            snapshots: store.snapshots.clone(),
        },
    );

    match serde_json::to_string_pretty(&on_disk) {
        Ok(data) => {
            // Sensitive-class data (cycle utilization per account) — mode 0600 on
            // Unix so other users on a shared host can't read it.
            if let Err(e) = atomic_write(&path, data.as_bytes(), Some(0o600)) {
                eprintln!("warning: failed to save snapshot: {e}");
            }
        }
        Err(e) => {
            eprintln!("warning: failed to serialize snapshot: {e}");
        }
    }
}

/// Round to the nearest minute for stable matching — the API returns
/// sub-second jitter in `resets_at` across calls.
fn round_to_minute(dt: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Timelike;
    dt.with_nanosecond(0)
        .and_then(|d: DateTime<Utc>| d.with_second(0))
        .unwrap_or(dt)
}

fn find_snapshot(store: &SnapshotStore, cycle_end: DateTime<Utc>) -> Option<f64> {
    let target = round_to_minute(cycle_end);
    // Prefer the most-recently-captured entry for this cycle: when a plan
    // change forked the cycle into multiple rows, the newer one reflects
    // current state.
    store
        .snapshots
        .iter()
        .filter(|s| round_to_minute(s.cycle_end) == target)
        .max_by_key(|s| s.captured_at)
        .map(|s| s.utilization)
}

fn current_cycle_from_snapshots(store: &SnapshotStore) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let latest = store.snapshots.iter().max_by_key(|s| s.cycle_end)?;
    let mut end = latest.cycle_end;
    let now = Utc::now();
    while end <= now {
        end += Duration::days(7);
    }
    Some((end - Duration::days(7), end))
}

fn compute_cycles(resets_at: DateTime<Utc>, count: usize) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut cycles = Vec::with_capacity(count);
    let mut end = resets_at;
    for _ in 0..count {
        let start = end - Duration::days(7);
        cycles.push((start, end));
        end = start;
    }
    cycles.reverse();
    cycles
}

fn cost_in_range(
    records: &[&UsageRecord],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    pricing: &dyn PricingMap,
) -> Option<f64> {
    let mut total = 0.0;
    for r in records {
        if r.timestamp >= start && r.timestamp < end {
            if let Some(c) = pricing.cost_for_record(r) {
                total += c;
            }
        }
    }
    Some(total)
}

fn format_tier(sub_type: &str, tier: &str) -> String {
    let multiplier = if tier.contains("20x") {
        "20x"
    } else if tier.contains("5x") {
        "5x"
    } else {
        ""
    };

    let plan = match sub_type {
        "max" => "Claude Max",
        "pro" => "Claude Pro",
        _ => sub_type,
    };

    if multiplier.is_empty() {
        plan.to_string()
    } else {
        format!("{plan} ({multiplier})")
    }
}

fn format_duration_short(hours: f64) -> String {
    let total_minutes = (hours * 60.0).round() as i64;
    let days = total_minutes / (24 * 60);
    let remaining = total_minutes % (24 * 60);
    let h = remaining / 60;
    let m = remaining % 60;
    match (days, h, m) {
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

// --- Plan recommendation ---

/// Claude subscription plans we can reason about.
/// Prices are Anthropic's public USD rates as of early 2026; may drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Plan {
    Pro,
    Max5x,
    Max20x,
}

impl Plan {
    /// Usage capacity expressed in "Pro units" — Anthropic describes
    /// Max as 5×/20× Pro's weekly limit.
    fn pro_units(self) -> f64 {
        match self {
            Plan::Pro => 1.0,
            Plan::Max5x => 5.0,
            Plan::Max20x => 20.0,
        }
    }

    fn price_usd(self) -> f64 {
        match self {
            Plan::Pro => 20.0,
            Plan::Max5x => 100.0,
            Plan::Max20x => 200.0,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Plan::Pro => "Claude Pro",
            Plan::Max5x => "Claude Max (5x)",
            Plan::Max20x => "Claude Max (20x)",
        }
    }

    fn downgrade(self) -> Option<Plan> {
        match self {
            Plan::Pro => None,
            Plan::Max5x => Some(Plan::Pro),
            Plan::Max20x => Some(Plan::Max5x),
        }
    }

    fn upgrade(self) -> Option<Plan> {
        match self {
            Plan::Pro => Some(Plan::Max5x),
            Plan::Max5x => Some(Plan::Max20x),
            Plan::Max20x => None,
        }
    }
}

fn detect_plan(oauth: &OAuthCredentials) -> Option<Plan> {
    let sub = oauth.subscription_type.as_deref()?;
    let tier = oauth.rate_limit_tier.as_deref().unwrap_or("");
    match sub {
        "pro" => Some(Plan::Pro),
        "max" if tier.contains("20x") => Some(Plan::Max20x),
        "max" if tier.contains("5x") => Some(Plan::Max5x),
        "max" => Some(Plan::Max5x),
        _ => None,
    }
}

#[derive(Debug)]
enum Recommendation {
    Stay,
    Downgrade(Plan),
    Upgrade(Plan),
}

/// Project a utilization % from `from` to `to`, scaled by Pro-unit capacity.
fn project_pct(pct: f64, from: Plan, to: Plan) -> f64 {
    pct * from.pro_units() / to.pro_units()
}

/// Decide whether to recommend a plan change.
/// - Upgrade: at least 2 of the recent cycles hit ≥95%, or avg ≥85%.
/// - Downgrade: peak cycle projected onto the lower plan stays ≤85%.
/// - Otherwise: stay.
fn recommend(current: Plan, utilizations: &[f64]) -> Recommendation {
    if utilizations.is_empty() {
        return Recommendation::Stay;
    }
    let avg = utilizations.iter().sum::<f64>() / utilizations.len() as f64;
    let max = utilizations.iter().cloned().fold(0.0_f64, f64::max);
    let near_cap = utilizations.iter().filter(|&&x| x >= 95.0).count();

    if let Some(higher) = current.upgrade() {
        if near_cap >= 2 || avg >= 85.0 {
            return Recommendation::Upgrade(higher);
        }
    }

    if let Some(lower) = current.downgrade() {
        if project_pct(max, current, lower) <= 85.0 {
            return Recommendation::Downgrade(lower);
        }
    }

    Recommendation::Stay
}

fn run_plan_mode(
    oauth: &OAuthCredentials,
    store: &SnapshotStore,
    current_cycle_end: DateTime<Utc>,
    exchange: &ExchangeRate,
    live_plan: Option<Plan>,
    usage: &Option<UsageResponse>,
) -> Result<()> {
    // Live profile is the source of truth; OAuth claims are the fallback.
    let Some(current) = live_plan.or_else(|| detect_plan(oauth)) else {
        eprintln!(
            "Unsupported subscription type: {:?}",
            oauth.subscription_type
        );
        eprintln!("Plan recommendations only cover Claude Pro and Max (5x / 20x).");
        std::process::exit(1);
    };

    // Use completed cycles only — exclude the in-progress current cycle.
    // Snapshots for past cycles retain the last captured utilization for that week.
    let target_end = round_to_minute(current_cycle_end);
    let mut completed: Vec<&CycleSnapshot> = store
        .snapshots
        .iter()
        .filter(|s| round_to_minute(s.cycle_end) != target_end)
        .collect();
    completed.sort_by_key(|s| s.cycle_end);
    let recent: Vec<&CycleSnapshot> = completed.iter().rev().take(4).rev().copied().collect();

    // Partition by provenance: native (current plan) drives the heuristic,
    // foreign goes to the table only (re-projected), unknown is excluded.
    let native: Vec<&CycleSnapshot> = recent
        .iter()
        .copied()
        .filter(|s| s.plan == Some(current))
        .collect();
    let foreign: Vec<&CycleSnapshot> = recent
        .iter()
        .copied()
        .filter(|s| matches!(s.plan, Some(p) if p != current))
        .collect();
    let mixed_plans = !foreign.is_empty() || recent.iter().any(|s| s.plan.is_none());

    eprintln!(
        "{} — {}/month",
        current.label(),
        exchange.format_cost(Some(current.price_usd()))
    );
    eprintln!();

    let native_utilizations: Vec<f64> = native.iter().map(|s| s.utilization).collect();
    let opus_pct = usage
        .as_ref()
        .and_then(|u| u.seven_day_opus.as_ref())
        .map(|w| w.utilization);

    enum Synth {
        None,
        Recent,                     // post-switch, no useful data yet
        Partial { projected: f64 }, // in-progress current cycle, ≥48h
    }

    let synth = if native.is_empty() {
        // Post-switch: try the in-progress cycle as a single synthetic point
        // when we have at least 48h of history on the new plan.
        let cycle_start = current_cycle_end - Duration::days(7);
        let elapsed_h = Utc::now().signed_duration_since(cycle_start).num_minutes() as f64 / 60.0;
        let live_pct = usage
            .as_ref()
            .and_then(|u| u.seven_day.as_ref())
            .map(|w| w.utilization);
        match (live_pct, elapsed_h >= 48.0) {
            (Some(pct), true) if pct > 0.0 => {
                let cycle_remaining_h = current_cycle_end
                    .signed_duration_since(Utc::now())
                    .num_minutes() as f64
                    / 60.0;
                let rate_per_h = pct / elapsed_h;
                let projected = (pct + rate_per_h * cycle_remaining_h).min(200.0);
                Synth::Partial { projected }
            }
            _ => Synth::Recent,
        }
    } else {
        Synth::None
    };

    let (avg, max, rec): (f64, f64, Recommendation) = if !native.is_empty() {
        let avg = native_utilizations.iter().sum::<f64>() / native_utilizations.len() as f64;
        let max = native_utilizations.iter().cloned().fold(0.0_f64, f64::max);
        let mut rec = recommend(current, &native_utilizations);
        // Opus-share guard: per-snapshot Opus history isn't stored, so we
        // only block on the live current-cycle figure. If Opus is hot,
        // Pro's tighter Opus tier would throttle hard; defer the downgrade.
        if let Recommendation::Downgrade(Plan::Pro) = rec {
            if let Some(opus) = opus_pct {
                if opus >= 25.0 {
                    eprintln!(
                        "Pro downgrade skipped — Opus usage on current cycle (~{:.0}%) suggests Pro's tier limits would throttle hard.",
                        opus
                    );
                    rec = Recommendation::Stay;
                }
            }
        }
        (avg, max, rec)
    } else if let Synth::Partial { projected } = synth {
        let utils = vec![projected];
        let mut rec = recommend(current, &utils);
        if let Recommendation::Downgrade(Plan::Pro) = rec {
            if let Some(opus) = opus_pct {
                if opus >= 25.0 {
                    eprintln!(
                        "Pro downgrade skipped — Opus usage on current cycle (~{:.0}%) suggests Pro's tier limits would throttle hard.",
                        opus
                    );
                    rec = Recommendation::Stay;
                }
            }
        }
        (projected, projected, rec)
    } else {
        (0.0, 0.0, Recommendation::Stay)
    };

    // Headline. Special-case the post-switch "no native data, no useful
    // partial cycle" case — show the table but defer the explanatory note
    // to *after* the table so the recommendation slot stays visually clean.
    let recent_switch = native.is_empty() && matches!(synth, Synth::Recent);
    let mut printed_recommendation = false;
    if recent_switch {
        // Note rendered post-table; nothing here.
    } else {
        printed_recommendation = true;
        let cycle_count_label = if native.is_empty() {
            "1 (partial cycle)".to_string()
        } else {
            format!("{}", native.len())
        };
        match rec {
            Recommendation::Downgrade(to) => {
                let savings = current.price_usd() - to.price_usd();
                let proj_avg = project_pct(avg, current, to);
                let proj_max = project_pct(max, current, to);
                eprintln!(
                    "▸ Recommend: downgrade to {} — save ~{}/month",
                    to.label(),
                    exchange.format_cost(Some(savings))
                );
                eprintln!();
                eprintln!(
                    "  {}-cycle average was {:.0}% (peak {:.0}%). On {}, this projects to",
                    cycle_count_label,
                    avg,
                    max,
                    to.label()
                );
                eprintln!(
                    "  ~{:.0}% avg (~{:.0}% peak) — comfortable headroom.",
                    proj_avg, proj_max
                );
            }
            Recommendation::Upgrade(to) => {
                let extra = to.price_usd() - current.price_usd();
                eprintln!(
                    "▸ Recommend: upgrade to {} — +{}/month",
                    to.label(),
                    exchange.format_cost(Some(extra))
                );
                eprintln!();
                let near_cap = native_utilizations.iter().filter(|&&x| x >= 95.0).count();
                if near_cap >= 2 {
                    eprintln!(
                        "  You've hit ≥95% utilization in {} of the last {} cycles.",
                        near_cap,
                        native.len()
                    );
                } else {
                    eprintln!(
                        "  {}-cycle average is {:.0}% — consistently near capacity.",
                        cycle_count_label, avg
                    );
                }
                eprintln!(
                    "  {} offers {:.0}× more headroom for the same workload.",
                    to.label(),
                    to.pro_units() / current.pro_units()
                );
            }
            Recommendation::Stay => {
                eprintln!("▸ Recommend: stay on {}", current.label());
                eprintln!();
                if native.is_empty() {
                    eprintln!(
                        "  Partial-cycle projection: ~{:.0}% on {}.",
                        max,
                        current.label()
                    );
                } else {
                    eprintln!(
                        "  {}-cycle average was {:.0}% (peak {:.0}%).",
                        cycle_count_label, avg, max
                    );
                }
                if let Some(lower) = current.downgrade() {
                    let proj_max = project_pct(max, current, lower);
                    eprintln!(
                        "  Downgrading to {} would push peak to ~{:.0}% — too tight.",
                        lower.label(),
                        proj_max
                    );
                } else {
                    eprintln!("  Utilization is in a comfortable range for this plan.");
                }
            }
        }
    }

    if native.is_empty() && matches!(synth, Synth::Recent) && recent.len() < 2 && foreign.is_empty()
    {
        // Fresh install or genuinely no data — preserve the original guidance.
        eprintln!();
        eprintln!("Run `tku sub` over a few weekly cycles first — recommendations need");
        eprintln!("at least 2 completed cycles to be meaningful.");
        return Ok(());
    }

    if printed_recommendation {
        eprintln!();
    }

    // Cycle table
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    // Ascending tier order, regardless of which is current. Current keeps a
    // bare label; others get an "on " prefix so the user can still spot it.
    const TIER_ORDER: [Plan; 3] = [Plan::Pro, Plan::Max5x, Plan::Max20x];
    let mut header = vec![Cell::new("Period")];
    if mixed_plans {
        header.push(Cell::new("Plan"));
    }
    for plan in TIER_ORDER {
        let label = if plan == current {
            plan.label().to_string()
        } else {
            format!("on {}", plan.label())
        };
        header.push(Cell::new(label));
    }
    table.set_header(header);

    for snap in &recent {
        let start = snap.cycle_end - Duration::days(7);
        let period_label = format!(
            "{} → {}",
            start.format("%b %-d"),
            snap.cycle_end.format("%b %-d")
        );
        // Re-project foreign snapshots onto current; unknown stay as `?`.
        let snap_plan = snap.plan;
        let on_current_pct: Option<f64> = match snap_plan {
            Some(p) if p == current => Some(snap.utilization),
            Some(p) => Some(project_pct(snap.utilization, p, current)),
            None => None,
        };

        let mut row = vec![Cell::new(&period_label)];
        if mixed_plans {
            let plan_cell = match snap_plan {
                Some(p) => p.label().to_string(),
                None => "?".to_string(),
            };
            row.push(Cell::new(plan_cell));
        }
        let project_from = on_current_pct;
        for plan in TIER_ORDER {
            let cell = match project_from {
                Some(p) if plan == current => format!("{:.0}%", p),
                Some(p) => format_projection(project_pct(p, current, plan)),
                None => "?".to_string(),
            };
            row.push(Cell::new(cell));
        }
        table.add_row(row);
    }

    println!("{table}");

    eprintln!();
    if printed_recommendation {
        let cycles_used = if native.is_empty() {
            "a partial cycle on the new plan".to_string()
        } else {
            format!(
                "{} completed weekly cycle{}",
                native.len(),
                if native.len() == 1 { "" } else { "s" }
            )
        };
        eprintln!("Based on {}. Seasonal patterns or", cycles_used);
        eprintln!("upcoming projects may shift your actual needs.");
    } else if recent_switch {
        eprintln!(
            "Note: just switched to {}. A recommendation will appear once",
            current.label()
        );
        eprintln!("there's ~48h of usage on the new plan, or one completed cycle.");
    }

    Ok(())
}

fn format_projection(pct: f64) -> String {
    if pct > 100.0 {
        format!(">100% (~{:.0}%)", pct)
    } else {
        format!("~{:.0}%", pct)
    }
}

fn format_overage(extra: Option<&ExtraUsage>, exchange: &ExchangeRate) -> String {
    let Some(extra) = extra else {
        return "—".to_string();
    };
    if !extra.is_enabled {
        return "disabled".to_string();
    }
    // API returns cents
    let used = extra.used_credits / 100.0;
    let limit = extra.monthly_limit / 100.0;
    format!(
        "{} / {}",
        exchange.format_cost(Some(used)),
        exchange.format_cost(Some(limit))
    )
}

// --- Cross-account view (`tku sub --all`) ---

/// One row in the cross-account overview. Each account has its own billing
/// cycle, plan, and OAuth token, so the only thing we sum across rows is
/// the cost column — usage % comparisons are deliberately left to the user.
struct AccountRow {
    name: String,
    is_active: bool,
    plan_label: String,
    seven_day_pct: Option<f64>,
    seven_day_source: UsageSource,
    five_hour_pct: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
    cost: Option<f64>,
    note: Option<String>,
}

/// Render a one-row-per-account overview of every registered Claude account.
///
/// Per-account behavior:
/// - Currently-active account uses live `~/.claude/.credentials.json` (fresh
///   token — Claude Code refreshes it on every run).
/// - Inactive accounts use their stashed credentials. If the stashed token
///   has expired, we skip the API call and fall back to the cached snapshot;
///   the user is told to switch in and re-auth to get fresh data.
/// - Cost is always computed locally from records; it works fully offline.
pub fn run_all(
    exchange: &ExchangeRate,
    records: &[UsageRecord],
    pricing: &dyn PricingMap,
    offline: bool,
    live: bool,
) -> Result<()> {
    let registry = crate::accounts::load_registry("claude");
    if registry.accounts.is_empty() {
        eprintln!("No accounts registered.");
        eprintln!();
        eprintln!("Run `tku sub` once to auto-register your current account, or");
        eprintln!("`tku account add <name>` to register additional accounts.");
        return Ok(());
    }

    let active_org = crate::accounts::current_claude_org_uuid()
        .or_else(|| registry.latest_switch().map(|s| s.org_uuid.clone()));

    let claude_records: Vec<&UsageRecord> = records
        .iter()
        .filter(|r| r.provider == Provider::Claude)
        .collect();

    // Live creds load once — cheap, and avoids re-reading + re-parsing per
    // active-account match (there's only ever one active, but this also
    // keeps the inactive branch from accidentally touching the live file).
    let live_creds = load_credentials().ok();

    let rows: Vec<AccountRow> = registry
        .accounts
        .iter()
        .map(|account| {
            let is_active = active_org.as_deref() == Some(account.org_uuid.as_str());
            gather_account_row(
                account,
                is_active,
                live_creds.clone(),
                &claude_records,
                &registry,
                pricing,
                offline,
                live,
            )
        })
        .collect();

    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        Cell::new("Account"),
        Cell::new("Plan"),
        Cell::new("7-day"),
        Cell::new("5h"),
        Cell::new("Resets"),
        Cell::new("Cost"),
    ]);

    let mut total_cost = 0.0;
    let mut any_cost = false;
    for row in &rows {
        let name_cell = if row.is_active {
            format!("* {}", row.name)
        } else {
            format!("  {}", row.name)
        };
        let name_with_note = match &row.note {
            Some(n) => format!("{name_cell}\n  ({n})"),
            None => name_cell,
        };

        let seven_str = match (row.seven_day_pct, row.seven_day_source) {
            (Some(p), UsageSource::Estimated) => format!("~{:.0}%", p),
            (Some(p), _) => format!("{:.0}%", p),
            (None, _) => "—".to_string(),
        };
        let five_str = match row.five_hour_pct {
            Some(p) => format!("{:.0}%", p),
            None => "—".to_string(),
        };
        let resets_str = row
            .resets_at
            .map(|r| {
                r.with_timezone(&chrono::Local)
                    .format("%b %-d, %-I:%M%P")
                    .to_string()
            })
            .unwrap_or_else(|| "—".to_string());
        let cost_str = exchange.format_cost(row.cost);
        if let Some(c) = row.cost {
            total_cost += c;
            any_cost = true;
        }

        table.add_row(vec![
            Cell::new(name_with_note),
            Cell::new(&row.plan_label),
            Cell::new(&seven_str),
            Cell::new(&five_str),
            Cell::new(&resets_str),
            Cell::new(&cost_str),
        ]);
    }

    // Summing usage % across accounts on different cycles isn't meaningful,
    // so the TOTAL row only fills the cost column. Skip the row entirely
    // when there's a single account — it would just duplicate the data.
    if rows.len() > 1 && any_cost {
        table.add_row(vec![
            Cell::new("TOTAL"),
            Cell::new(""),
            Cell::new(""),
            Cell::new(""),
            Cell::new(""),
            Cell::new(exchange.format_cost(Some(total_cost))),
        ]);
    }

    println!("{table}");
    eprintln!();
    eprintln!("* = currently active. Run `tku sub --account <name>` for full details.");
    let _ = live; // currently identical to default per-account behavior; kept
                  // for parity with `tku sub` and a future force-refresh path.

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn gather_account_row(
    account: &crate::accounts::Account,
    is_active: bool,
    live_creds: Option<Credentials>,
    claude_records: &[&UsageRecord],
    registry: &crate::accounts::Registry,
    pricing: &dyn PricingMap,
    offline: bool,
    _live: bool,
) -> AccountRow {
    // Records belonging to this account: prefer the recorded uuid, fall
    // back to the timestamp-based switch log for legacy untagged records.
    let account_records: Vec<&UsageRecord> = claude_records
        .iter()
        .copied()
        .filter(|r| match r.account_uuid.as_deref() {
            Some(uuid) => uuid == account.org_uuid.as_str(),
            None => registry
                .account_at(r.timestamp)
                .map(|e| e.org_uuid == account.org_uuid)
                .unwrap_or(false),
        })
        .collect();

    let creds = if is_active {
        live_creds
    } else {
        let path = crate::accounts::stashed_creds_path("claude", &account.name);
        path.filter(|p| p.exists())
            .and_then(|p| fs::read_to_string(&p).ok())
            .and_then(|d| serde_json::from_str::<Credentials>(&d).ok())
    };

    let oauth = creds.as_ref().and_then(|c| c.claude_ai_oauth.as_ref());

    let now_ms = Utc::now().timestamp_millis() as u64;
    let creds_fresh = oauth.map(|o| now_ms <= o.expires_at).unwrap_or(false);

    let mut store = load_snapshots(&account.org_uuid);
    let cached_cycle = current_cycle_from_snapshots(&store);

    // Live profile only for the active account — inactive tokens may be
    // stale and we don't want to spam the API per-account on every run.
    let live_plan = if is_active && creds_fresh {
        oauth.and_then(|o| resolve_live_plan(&o.access_token, &account.org_uuid, offline).0)
    } else {
        None
    };

    // Plan label: live profile (active) → most-recent snapshot's tagged plan
    //  → OAuth claims → registry-stamped subscription_type at registration time.
    let plan_label = if let Some(p) = live_plan {
        p.label().to_string()
    } else if let Some(p) = store
        .snapshots
        .iter()
        .max_by_key(|s| s.captured_at)
        .and_then(|s| s.plan)
    {
        p.label().to_string()
    } else {
        match oauth {
            Some(o) => format_tier(
                o.subscription_type.as_deref().unwrap_or("unknown"),
                o.rate_limit_tier.as_deref().unwrap_or(""),
            ),
            None => match &account.subscription_type {
                Some(s) => format_tier(s, account.rate_limit_tier.as_deref().unwrap_or("")),
                None => "unknown".to_string(),
            },
        }
    };

    // Fetch unless we're offline or the token's stale. Inactive accounts
    // with stashed-but-expired tokens degrade gracefully to cached data —
    // we don't want a dead account to break the whole overview.
    let usage = if offline || !creds_fresh {
        None
    } else {
        oauth.and_then(|o| fetch_usage(&o.access_token).ok())
    };

    let resolved_plan = live_plan.or_else(|| oauth.and_then(detect_plan));

    let mut note = None;
    if !creds_fresh && oauth.is_some() && !is_active {
        note = Some("token expired — `tku account use` to refresh".to_string());
    } else if oauth.is_none() && !is_active {
        note = Some("no stashed credentials".to_string());
    }

    let seven_day = usage.as_ref().and_then(|u| u.seven_day.as_ref());
    let resets_at = seven_day
        .and_then(|w| w.resets_at.as_ref())
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .or_else(|| cached_cycle.map(|(_, end)| end));

    let cost = match resets_at {
        Some(end) => {
            let start = end - Duration::days(7);
            let upper = end.min(Utc::now());
            cost_in_range(&account_records, start, upper, pricing)
        }
        None => None,
    };

    // Persist fresh API data under this account's UUID so a later `--all`
    // run can show the % even if this account's token has since expired.
    if let (Some(end), Some(w)) = (resets_at, &seven_day) {
        save_snapshot(end, w.utilization, cost, resolved_plan, &mut store);
    }

    // Reuse the same Live → Estimated → Cached resolution the single-account
    // view uses, so a cycle with cached snapshot + cost data shows the
    // "~XX%" estimate instead of the stale calibration value.
    let (seven_day_pct, seven_day_source) = match resets_at {
        Some(end) => {
            resolve_current_usage(usage.as_ref(), &store, end, &account_records, pricing, cost)
        }
        None => (None, UsageSource::Cached),
    };

    let five_hour_pct = usage
        .as_ref()
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.utilization);

    AccountRow {
        name: account.name.clone(),
        is_active,
        plan_label,
        seven_day_pct,
        seven_day_source,
        five_hour_pct,
        resets_at,
        cost,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1_SAMPLE: &str = r#"{
        "snapshots": [
            {
                "cycle_end": "2026-04-01T10:00:00Z",
                "utilization": 18.0,
                "captured_at": "2026-03-28T10:00:00Z",
                "cost_at_calibration": 42.5
            },
            {
                "cycle_end": "2026-04-08T10:00:00Z",
                "utilization": 22.0,
                "captured_at": "2026-04-05T10:00:00Z",
                "cost_at_calibration": 51.0
            }
        ]
    }"#;

    const V2_SAMPLE: &str = r#"{
        "version": 2,
        "accounts": {
            "org-aaa": {
                "snapshots": [
                    {
                        "cycle_end": "2026-04-01T10:00:00Z",
                        "utilization": 18.0,
                        "captured_at": "2026-03-28T10:00:00Z",
                        "cost_at_calibration": 42.5
                    }
                ]
            },
            "org-bbb": {
                "snapshots": []
            }
        }
    }"#;

    #[test]
    fn migrates_v1_to_v2_under_current_org() {
        let store = parse_on_disk(V1_SAMPLE, "org-abc-123");
        assert_eq!(store.version, 2);
        assert_eq!(store.accounts.len(), 1);
        let bucket = store
            .accounts
            .get("org-abc-123")
            .expect("v1 snapshots should migrate under migration_org key");
        assert_eq!(bucket.snapshots.len(), 2);
        assert_eq!(bucket.snapshots[0].utilization, 18.0);
        assert_eq!(bucket.snapshots[0].cost_at_calibration, Some(42.5));
        assert_eq!(bucket.snapshots[1].utilization, 22.0);
    }

    #[test]
    fn preserves_v2_format_ignoring_migration_org() {
        let store = parse_on_disk(V2_SAMPLE, "org-should-be-ignored");
        assert_eq!(store.version, 2);
        assert_eq!(store.accounts.len(), 2);
        assert!(store.accounts.contains_key("org-aaa"));
        assert!(store.accounts.contains_key("org-bbb"));
        assert!(!store.accounts.contains_key("org-should-be-ignored"));
        assert_eq!(store.accounts["org-aaa"].snapshots.len(), 1);
        assert_eq!(store.accounts["org-bbb"].snapshots.len(), 0);
    }

    #[test]
    fn handles_empty_input() {
        let store = parse_on_disk("", "org-abc");
        assert_eq!(store.version, 2);
        assert!(store.accounts.is_empty());
    }

    #[test]
    fn handles_corrupt_input() {
        let store = parse_on_disk("not valid json", "org-abc");
        assert_eq!(store.version, 2);
        assert!(store.accounts.is_empty());
    }

    #[test]
    fn migration_roundtrips_through_v2() {
        // v1 → parse → serialize → parse again should land on identical v2 content.
        let store = parse_on_disk(V1_SAMPLE, "org-abc-123");
        let serialized = serde_json::to_string_pretty(&store).expect("serialize");
        let reparsed = parse_on_disk(&serialized, "org-should-not-matter-now");
        assert_eq!(reparsed.version, 2);
        assert_eq!(reparsed.accounts.len(), 1);
        assert_eq!(reparsed.accounts["org-abc-123"].snapshots.len(), 2);
    }

    // --- Plan serde + provenance tests ---

    #[test]
    fn plan_serializes_to_stable_strings() {
        assert_eq!(serde_json::to_string(&Plan::Pro).unwrap(), "\"pro\"");
        assert_eq!(serde_json::to_string(&Plan::Max5x).unwrap(), "\"max5x\"");
        assert_eq!(serde_json::to_string(&Plan::Max20x).unwrap(), "\"max20x\"");
    }

    #[test]
    fn plan_roundtrips_through_serde() {
        for p in [Plan::Pro, Plan::Max5x, Plan::Max20x] {
            let s = serde_json::to_string(&p).unwrap();
            let back: Plan = serde_json::from_str(&s).unwrap();
            assert_eq!(p, back);
        }
    }

    const V2_WITH_PLAN: &str = r#"{
        "version": 2,
        "accounts": {
            "org-aaa": {
                "snapshots": [
                    {
                        "cycle_end": "2026-04-01T10:00:00Z",
                        "utilization": 18.0,
                        "captured_at": "2026-03-28T10:00:00Z",
                        "cost_at_calibration": 42.5,
                        "plan": "max5x"
                    }
                ]
            }
        }
    }"#;

    #[test]
    fn snapshot_with_plan_roundtrips() {
        let store = parse_on_disk(V2_WITH_PLAN, "ignored");
        let snap = &store.accounts["org-aaa"].snapshots[0];
        assert_eq!(snap.plan, Some(Plan::Max5x));

        let serialized = serde_json::to_string_pretty(&store).expect("serialize");
        let reparsed = parse_on_disk(&serialized, "ignored");
        let snap2 = &reparsed.accounts["org-aaa"].snapshots[0];
        assert_eq!(snap2.plan, Some(Plan::Max5x));
        assert_eq!(snap2.utilization, 18.0);
    }

    #[test]
    fn snapshot_without_plan_field_loads_as_none() {
        // V2_SAMPLE has no `plan` field on its snapshot.
        let store = parse_on_disk(V2_SAMPLE, "ignored");
        let snap = &store.accounts["org-aaa"].snapshots[0];
        assert_eq!(snap.plan, None);
        // And re-serializes cleanly.
        let serialized = serde_json::to_string(&store).expect("serialize");
        assert!(serialized.contains("org-aaa"));
    }

    // --- recommend() native-only correctness ---

    #[test]
    fn recommend_with_natives_preserves_existing_behavior() {
        // High avg → upgrade.
        let rec = recommend(Plan::Max5x, &[88.0, 90.0, 92.0, 87.0]);
        assert!(matches!(rec, Recommendation::Upgrade(Plan::Max20x)));

        // Two near-cap cycles → upgrade.
        let rec = recommend(Plan::Pro, &[96.0, 95.0, 60.0]);
        assert!(matches!(rec, Recommendation::Upgrade(Plan::Max5x)));

        // Comfortable headroom on a lower plan → downgrade.
        // Max5x(15%) projected onto Pro = 15 * 5 = 75% ≤ 85 → downgrade.
        let rec = recommend(Plan::Max5x, &[10.0, 12.0, 15.0]);
        assert!(matches!(rec, Recommendation::Downgrade(Plan::Pro)));

        // Stay: utilization in mid-band on Max5x.
        // peak=50 → on Pro = 250% > 85 → no downgrade. avg=43 < 85 → no upgrade.
        let rec = recommend(Plan::Max5x, &[40.0, 50.0, 40.0]);
        assert!(matches!(rec, Recommendation::Stay));
    }

    #[test]
    fn recommend_with_empty_returns_stay() {
        let rec = recommend(Plan::Max5x, &[]);
        assert!(matches!(rec, Recommendation::Stay));
    }

    #[test]
    fn run_plan_native_only_filter_skips_foreign_evidence() {
        // The yo-yo regression: comfortable old-plan cycles must not
        // trigger an immediate re-upgrade after a downgrade.
        // We exercise the partition by simulating what `run_plan_mode`
        // does: feed only natives into recommend().
        let snaps = vec![
            // Foreign (pre-downgrade Max5x cycles, very low utilization)
            CycleSnapshot {
                cycle_end: "2026-04-01T10:00:00Z".parse().unwrap(),
                utilization: 10.0,
                captured_at: "2026-03-28T10:00:00Z".parse().unwrap(),
                cost_at_calibration: None,
                plan: Some(Plan::Max5x),
            },
            CycleSnapshot {
                cycle_end: "2026-04-08T10:00:00Z".parse().unwrap(),
                utilization: 12.0,
                captured_at: "2026-04-05T10:00:00Z".parse().unwrap(),
                cost_at_calibration: None,
                plan: Some(Plan::Max5x),
            },
        ];
        let current = Plan::Pro;
        let native: Vec<&CycleSnapshot> =
            snaps.iter().filter(|s| s.plan == Some(current)).collect();
        let utils: Vec<f64> = native.iter().map(|s| s.utilization).collect();
        let rec = recommend(current, &utils);
        // No native data → Stay, regardless of the comfortable foreign cycles.
        assert!(matches!(rec, Recommendation::Stay));
    }

    #[test]
    fn recommend_mixed_plans_uses_only_natives() {
        let snaps = vec![
            // Foreign pre-upgrade Pro cycles at 96% — would trigger upgrade
            // if naively included, but they're foreign now.
            CycleSnapshot {
                cycle_end: "2026-04-01T10:00:00Z".parse().unwrap(),
                utilization: 96.0,
                captured_at: "2026-03-28T10:00:00Z".parse().unwrap(),
                cost_at_calibration: None,
                plan: Some(Plan::Pro),
            },
            CycleSnapshot {
                cycle_end: "2026-04-08T10:00:00Z".parse().unwrap(),
                utilization: 95.0,
                captured_at: "2026-04-05T10:00:00Z".parse().unwrap(),
                cost_at_calibration: None,
                plan: Some(Plan::Pro),
            },
            // Native Max5x cycles at moderate utilization.
            CycleSnapshot {
                cycle_end: "2026-04-15T10:00:00Z".parse().unwrap(),
                utilization: 30.0,
                captured_at: "2026-04-12T10:00:00Z".parse().unwrap(),
                cost_at_calibration: None,
                plan: Some(Plan::Max5x),
            },
            CycleSnapshot {
                cycle_end: "2026-04-22T10:00:00Z".parse().unwrap(),
                utilization: 35.0,
                captured_at: "2026-04-19T10:00:00Z".parse().unwrap(),
                cost_at_calibration: None,
                plan: Some(Plan::Max5x),
            },
        ];
        let current = Plan::Max5x;
        let utils: Vec<f64> = snaps
            .iter()
            .filter(|s| s.plan == Some(current))
            .map(|s| s.utilization)
            .collect();
        let rec = recommend(current, &utils);
        // peak=35% on Max5x → on Pro = 175% > 85, can't downgrade.
        // avg=32.5% → no upgrade. → Stay.
        assert!(matches!(rec, Recommendation::Stay));
    }

    #[test]
    fn save_snapshot_forks_on_plan_change() {
        // Same cycle_end, different plans → two entries, not one mutation.
        let mut snaps: Vec<CycleSnapshot> = Vec::new();
        let cycle_end: DateTime<Utc> = "2026-04-15T10:00:00Z".parse().unwrap();
        let t1: DateTime<Utc> = "2026-04-13T10:00:00Z".parse().unwrap();
        let t2: DateTime<Utc> = "2026-04-14T10:00:00Z".parse().unwrap();

        apply_snapshot(
            &mut snaps,
            cycle_end,
            40.0,
            Some(120.0),
            Some(Plan::Max5x),
            t1,
        );
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].plan, Some(Plan::Max5x));

        apply_snapshot(
            &mut snaps,
            cycle_end,
            80.0,
            Some(180.0),
            Some(Plan::Pro),
            t2,
        );
        assert_eq!(snaps.len(), 2, "plan change should fork rather than mutate");

        // Original Max5x entry preserved as-is.
        let max5x = snaps
            .iter()
            .find(|s| s.plan == Some(Plan::Max5x))
            .expect("max5x entry preserved");
        assert_eq!(max5x.utilization, 40.0);
        assert_eq!(max5x.captured_at, t1);

        // New Pro entry appended.
        let pro = snaps
            .iter()
            .find(|s| s.plan == Some(Plan::Pro))
            .expect("pro entry appended");
        assert_eq!(pro.utilization, 80.0);
        assert_eq!(pro.captured_at, t2);
    }

    #[test]
    fn save_snapshot_updates_in_place_when_plan_unchanged() {
        let mut snaps: Vec<CycleSnapshot> = Vec::new();
        let cycle_end: DateTime<Utc> = "2026-04-15T10:00:00Z".parse().unwrap();
        let t1: DateTime<Utc> = "2026-04-13T10:00:00Z".parse().unwrap();
        let t2: DateTime<Utc> = "2026-04-14T10:00:00Z".parse().unwrap();
        apply_snapshot(&mut snaps, cycle_end, 40.0, None, Some(Plan::Max5x), t1);
        apply_snapshot(&mut snaps, cycle_end, 55.0, None, Some(Plan::Max5x), t2);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].utilization, 55.0);
        assert_eq!(snaps[0].captured_at, t2);
    }
}
