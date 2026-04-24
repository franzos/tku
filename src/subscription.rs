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
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

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

// --- OAuth credentials ---

#[derive(Debug, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OAuthCredentials>,
    #[serde(rename = "organizationUuid")]
    organization_uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
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
) -> Result<()> {
    let creds = match load_credentials() {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Claude Code credentials not found (~/.claude/.credentials.json).");
            eprintln!("Run Claude Code at least once to create them.");
            eprintln!();
            eprintln!("The subscription command currently only supports Claude Max/Pro.");
            std::process::exit(1);
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

    // `organizationUuid` is not always present in `.credentials.json` — Claude
    // Code drops it when refreshing the access token. Fall back to the most
    // recent active account in our registry so snapshots don't get stranded
    // under an "unknown" key.
    let org_uuid = creds
        .organization_uuid
        .or_else(|| {
            crate::accounts::load_registry("claude")
                .latest_switch()
                .map(|s| s.org_uuid.clone())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let now_ms = Utc::now().timestamp_millis() as u64;
    if now_ms > oauth.expires_at {
        eprintln!("Claude OAuth token expired. Run Claude Code to refresh your session.");
        std::process::exit(1);
    }

    // Filter records to Claude only
    let claude_records: Vec<&UsageRecord> = records
        .iter()
        .filter(|r| r.provider == Provider::Claude)
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

    // Save snapshot for current cycle when we got live data
    if let Some(w) = &seven_day {
        save_snapshot(resets_at, w.utilization, current_cost, &mut store);
    }

    if plan {
        return run_plan_mode(&oauth, &store, resets_at, exchange);
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

    // Header
    let sub_type = oauth.subscription_type.as_deref().unwrap_or("unknown");
    let tier = oauth.rate_limit_tier.as_deref().unwrap_or("");
    let tier_label = format_tier(sub_type, tier);

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

fn save_snapshot(
    cycle_end: DateTime<Utc>,
    utilization: f64,
    cost: Option<f64>,
    store: &mut SnapshotStore,
) {
    let Some(path) = snapshot_path() else { return };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("warning: failed to create snapshot dir: {e}");
        }
    }

    let target = round_to_minute(cycle_end);
    if let Some(existing) = store
        .snapshots
        .iter_mut()
        .find(|s| round_to_minute(s.cycle_end) == target)
    {
        existing.cycle_end = cycle_end;
        existing.utilization = utilization;
        existing.captured_at = Utc::now();
        existing.cost_at_calibration = cost;
    } else {
        store.snapshots.push(CycleSnapshot {
            cycle_end,
            utilization,
            captured_at: Utc::now(),
            cost_at_calibration: cost,
        });
    }

    // Keep only last 12 snapshots per account
    store.snapshots.sort_by_key(|s| s.cycle_end);
    if store.snapshots.len() > 12 {
        let excess = store.snapshots.len() - 12;
        store.snapshots.drain(..excess);
    }

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
    store
        .snapshots
        .iter()
        .find(|s| round_to_minute(s.cycle_end) == target)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
) -> Result<()> {
    let Some(current) = detect_plan(oauth) else {
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

    eprintln!(
        "{} — {}/month",
        current.label(),
        exchange.format_cost(Some(current.price_usd()))
    );
    eprintln!();

    if recent.len() < 2 {
        eprintln!(
            "Not enough data for a recommendation ({} completed cycle{} captured).",
            recent.len(),
            if recent.len() == 1 { "" } else { "s" }
        );
        eprintln!("Run `tku sub` over a few weekly cycles first — recommendations need");
        eprintln!("at least 2 completed cycles to be meaningful.");
        return Ok(());
    }

    let utilizations: Vec<f64> = recent.iter().map(|s| s.utilization).collect();
    let avg = utilizations.iter().sum::<f64>() / utilizations.len() as f64;
    let max = utilizations.iter().cloned().fold(0.0_f64, f64::max);
    let rec = recommend(current, &utilizations);

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
                recent.len(),
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
            let near_cap = utilizations.iter().filter(|&&x| x >= 95.0).count();
            if near_cap >= 2 {
                eprintln!(
                    "  You've hit ≥95% utilization in {} of the last {} cycles.",
                    near_cap,
                    recent.len()
                );
            } else {
                eprintln!(
                    "  {}-cycle average is {:.0}% — consistently near capacity.",
                    recent.len(),
                    avg
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
            eprintln!(
                "  {}-cycle average was {:.0}% (peak {:.0}%).",
                recent.len(),
                avg,
                max
            );
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

    eprintln!();

    // Cycle table
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    let mut header = vec![Cell::new("Period"), Cell::new(current.label())];
    if let Some(lower) = current.downgrade() {
        header.push(Cell::new(format!("on {}", lower.label())));
        if let Some(lower2) = lower.downgrade() {
            header.push(Cell::new(format!("on {}", lower2.label())));
        }
    }
    if let Some(higher) = current.upgrade() {
        header.push(Cell::new(format!("on {}", higher.label())));
    }
    table.set_header(header);

    for snap in &recent {
        let start = snap.cycle_end - Duration::days(7);
        let period_label = format!(
            "{} → {}",
            start.format("%b %-d"),
            snap.cycle_end.format("%b %-d")
        );
        let pct = snap.utilization;
        let mut row = vec![Cell::new(&period_label), Cell::new(format!("{:.0}%", pct))];
        if let Some(lower) = current.downgrade() {
            row.push(Cell::new(format_projection(project_pct(
                pct, current, lower,
            ))));
            if let Some(lower2) = lower.downgrade() {
                row.push(Cell::new(format_projection(project_pct(
                    pct, current, lower2,
                ))));
            }
        }
        if let Some(higher) = current.upgrade() {
            row.push(Cell::new(format_projection(project_pct(
                pct, current, higher,
            ))));
        }
        table.add_row(row);
    }

    println!("{table}");

    eprintln!();
    eprintln!(
        "Based on {} completed weekly cycle{}. Seasonal patterns or",
        recent.len(),
        if recent.len() == 1 { "" } else { "s" }
    );
    eprintln!("upcoming projects may shift your actual needs.");

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
}
