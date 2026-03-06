use std::fs;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, ContentArrangement, Table};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

use crate::cost::PricingMap;
use crate::exchange::ExchangeRate;
use crate::types::UsageRecord;

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

#[derive(Debug, Serialize, Deserialize, Default)]
struct SnapshotStore {
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
#[derive(Clone, Copy)]
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

    let now_ms = Utc::now().timestamp_millis() as u64;
    if now_ms > oauth.expires_at {
        eprintln!("Claude OAuth token expired. Run Claude Code to refresh your session.");
        std::process::exit(1);
    }

    // Filter records to Claude only
    let claude_records: Vec<&UsageRecord> =
        records.iter().filter(|r| r.provider == "claude").collect();

    let mut store = load_snapshots();

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
            Err(e) => {
                eprintln!("Warning: failed to fetch usage: {e}");
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

    let cycles = compute_cycles(resets_at, 4);

    // Resolve current week's utilization + source
    let (current_pct, current_source) = resolve_current_usage(
        &usage,
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
    usage: &Option<UsageResponse>,
    store: &SnapshotStore,
    cycle_end: DateTime<Utc>,
    records: &[&UsageRecord],
    pricing: &dyn PricingMap,
    current_cost: Option<f64>,
) -> (Option<f64>, UsageSource) {
    // Live data takes priority
    if let Some(ref u) = usage {
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
        fs::read_to_string(&path).with_context(|| format!("Cannot read {}", path.display()))?;
    serde_json::from_str(&data).context("Failed to parse credentials")
}

fn fetch_usage(access_token: &str) -> Result<UsageResponse> {
    let body = ureq::get(USAGE_API_URL)
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
    ProjectDirs::from("", "", "tku").map(|d| d.cache_dir().join("subscription-claude.json"))
}

fn load_snapshots() -> SnapshotStore {
    let Some(path) = snapshot_path() else {
        return SnapshotStore::default();
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return SnapshotStore::default();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_snapshot(
    cycle_end: DateTime<Utc>,
    utilization: f64,
    cost: Option<f64>,
    store: &mut SnapshotStore,
) {
    let Some(path) = snapshot_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
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

    // Keep only last 12 snapshots
    store.snapshots.sort_by_key(|s| s.cycle_end);
    if store.snapshots.len() > 12 {
        let excess = store.snapshots.len() - 12;
        store.snapshots.drain(..excess);
    }

    if let Ok(data) = serde_json::to_string_pretty(&store) {
        let _ = fs::write(&path, data);
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
