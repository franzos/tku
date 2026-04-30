mod accounts;
mod aggregate;
mod atomic_write;
mod cli;
mod config;
mod cost;
mod dedup;
mod exchange;
mod graph;
mod http;
mod output;
mod paths;
mod pricing;
mod providers;
mod storage;
mod subscription;
mod types;
mod watch;

use std::io::Write;
use std::str::FromStr;

use anyhow::{bail, Result};
use chrono::Datelike;
use clap::Parser;

use cli::Cli;
use cost::PricingMap;
use types::Provider;

fn bar_date_range(period: &cli::BarPeriod) -> (chrono::NaiveDate, chrono::NaiveDate) {
    let today = chrono::Local::now().date_naive();
    match period {
        cli::BarPeriod::Today => (today, today),
        cli::BarPeriod::Week => (today - chrono::Duration::days(6), today),
        cli::BarPeriod::Month => {
            let first =
                chrono::NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);
            (first, today)
        }
    }
}

fn bar_period_label(period: &cli::BarPeriod) -> &'static str {
    match period {
        cli::BarPeriod::Today => "Today",
        cli::BarPeriod::Week => "Week",
        cli::BarPeriod::Month => "Month",
    }
}

/// Does this record belong to `account`?
/// None account = no filter, always matches.
///
/// Two-stage attribution:
/// 1. If the record carries an `account_uuid` (tagged at scan time from the
///    live credentials file), match directly against the registry's
///    name → uuid mapping. This is the precise path.
/// 2. If no uuid is set (legacy cache, or creds were unreadable at scan
///    time), fall back to the timestamp-based switch log via `account_at`.
fn matches_account(
    record: &types::UsageRecord,
    account: Option<&str>,
    registry: &accounts::Registry,
) -> bool {
    let Some(name) = account else {
        return true;
    };
    if record.provider != Provider::Claude {
        return false;
    }
    if let Some(uuid) = record.account_uuid.as_deref() {
        // Prefer the recorded uuid → name mapping. Fall through to
        // switch-log lookup only when the uuid isn't yet registered (e.g. an
        // implicit-detected account the user hasn't named via `account add`).
        if let Some(a) = registry.find_by_org(uuid) {
            return a.name == *name;
        }
    }
    registry
        .account_at(record.timestamp)
        .map(|e| e.name == *name)
        .unwrap_or(false)
}

fn handle_account(action: &cli::AccountAction) -> Result<()> {
    match action {
        cli::AccountAction::Add { name } => accounts::add(name),
        cli::AccountAction::Use { name, force } => accounts::use_account(name, *force),
        cli::AccountAction::List => accounts::list(),
        cli::AccountAction::Current => accounts::current(),
        cli::AccountAction::Rename { old, new } => accounts::rename(old, new),
        cli::AccountAction::Remove { name, force } => accounts::remove(name, *force),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mode = cli.effective_command();

    // Account management subcommands: handled early; no record scan needed.
    if let cli::Command::Account { action } = &mode {
        return handle_account(action);
    }

    let config = config::load_config();

    // Merge: CLI > config > default
    let pricing_source = cli
        .pricing_source
        .clone()
        .or(config.pricing_source)
        .unwrap_or_default();

    let currency = cli
        .currency
        .clone()
        .or(config.currency)
        .unwrap_or_else(|| "USD".to_string());

    let currency = currency.to_uppercase();
    if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_uppercase()) {
        bail!(
            "Invalid currency code: '{}'. Expected 3-letter ISO 4217 code (e.g. EUR, GBP).",
            currency
        );
    }

    let is_bar = matches!(mode, cli::Command::Bar { .. });
    let is_plot = matches!(mode, cli::Command::Plot { .. });
    let is_sub = matches!(mode, cli::Command::Subscription { .. });

    let date_range = if let cli::Command::Plot { ref period, .. } = mode {
        let today = chrono::Local::now().date_naive();
        let days_back = match period {
            cli::GraphPeriod::Day => 1,
            cli::GraphPeriod::Week => 7,
            cli::GraphPeriod::Month => 30,
        };
        Some((today - chrono::Duration::days(days_back), today))
    } else if let cli::Command::Bar { ref period, .. } = mode {
        Some(bar_date_range(period))
    } else if matches!(mode, cli::Command::Subscription { .. })
        && cli.from.is_none()
        && cli.to.is_none()
    {
        let today = chrono::Local::now().date_naive();
        Some((today - chrono::Duration::days(35), today))
    } else if matches!(mode, cli::Command::Watch { .. }) && cli.from.is_none() && cli.to.is_none() {
        let today = chrono::Local::now().date_naive();
        Some((today, today))
    } else {
        match (cli.from, cli.to) {
            (Some(f), Some(t)) => Some((f, t)),
            (Some(f), None) => Some((f, chrono::Utc::now().date_naive())),
            (None, Some(t)) => Some((chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap_or(t), t)),
            (None, None) => None,
        }
    };

    if let cli::Command::Watch { full, interval } = mode {
        return watch::run(full, interval, &cli, &pricing_source, &currency, date_range);
    }

    // Pre-scan: detect implicit credential swap so the registry's switch log
    // already reflects any external swap before we tag records. Without
    // this, per-record account_at(timestamp) lookups would resolve new
    // records to the previous account.
    accounts::detect_implicit_swap_pre_scan();

    let mut store = storage::default_storage();

    let show_progress = !cli.cli && !is_bar && !is_plot && !is_sub;
    // Throttle is applied inside providers::discover_and_parse_with so the
    // per-file hot path stays tight. This callback itself can be cheap.
    let progress_cb = |current: usize, total: usize| {
        eprint!("\x1b[2K\rScanning sessions... {current}/{total}");
        let _ = std::io::stderr().flush();
    };
    for provider in providers::all_providers() {
        provider.discover_and_parse(
            store.as_mut(),
            if show_progress {
                Some(&progress_cb)
            } else {
                None
            },
            cli.prune,
        );
    }
    if show_progress {
        eprint!("\x1b[2K\r");
        let _ = std::io::stderr().flush();
    }

    store.flush();
    let all_records = store.drain_all();

    let records = dedup::dedup(all_records);

    // Post-scan bootstrap: register the active credentials as "default" on
    // first run. Implicit-swap detection already ran pre-scan; this only
    // fires when the registry is still empty.
    let claude_refs: Vec<&types::UsageRecord> = records
        .iter()
        .filter(|r| r.provider == Provider::Claude)
        .collect();
    let _ = accounts::bootstrap_if_needed_post_scan(&claude_refs);
    drop(claude_refs);

    let account_filter = cli.account.clone();
    let account_registry = accounts::load_registry("claude");

    if let cli::Command::Subscription { live, plan, all } = mode {
        // `--all` shows every account; `--account <name>` scopes to one.
        // Combining them is contradictory, so reject the combination instead
        // of silently picking one (clap can't enforce this directly because
        // `--account` is global and `--all` is subcommand-local).
        if all && account_filter.is_some() {
            bail!("`--all` and `--account` are mutually exclusive");
        }
        let exchange = exchange::load_exchange_rate(&currency, cli.offline);
        let pricing = pricing::load_pricing(&pricing_source, cli.offline)?;
        if all {
            return subscription::run_all(&exchange, &records, &pricing, cli.offline, live);
        }
        // Pass the unfiltered record set into `subscription::run` along with
        // the `--account` selector. The subscription command needs to load
        // the matching account's stashed credentials to fetch usage from the
        // API — filtering records out here would still leave the API call
        // hitting the wrong account's token.
        return subscription::run(
            &exchange,
            &records,
            &pricing,
            cli.offline,
            live,
            plan,
            account_filter.as_deref(),
        );
    }

    let proj_needle = cli.project.as_ref().map(|p| p.to_lowercase());
    // Parse the `--tool` string into a typed Provider up front. Three states:
    //   None           → no --tool flag, keep all records
    //   Some(Ok(p))    → filter to this provider
    //   Some(Err(()))  → --tool was set but name doesn't map to a Provider,
    //                    so no records should match (loud-fail alternative
    //                    would be bail! here; we keep the pre-refactor
    //                    silent-drop semantics).
    let tool_needle: Option<Result<Provider, ()>> = cli
        .tool
        .as_ref()
        .map(|t| Provider::from_str(t).map_err(|_| ()));

    let records: Vec<_> = records
        .into_iter()
        .filter(|r| match date_range {
            Some((from, to)) => {
                let date = r.timestamp.date_naive();
                date >= from && date <= to
            }
            None => true,
        })
        .filter(|r| match &proj_needle {
            Some(needle) => r.project.to_lowercase().contains(needle),
            None => true,
        })
        .filter(|r| match tool_needle {
            None => true,
            Some(Ok(needle)) => r.provider == needle,
            Some(Err(())) => false,
        })
        .filter(|r| matches_account(r, account_filter.as_deref(), &account_registry))
        .collect();

    if let cli::Command::Plot {
        ref period,
        relative,
    } = mode
    {
        return graph::render(&records, period, relative);
    }

    let exchange = exchange::load_exchange_rate(&currency, cli.offline);

    if let cli::Command::Bar {
        ref period,
        ref template,
        warn,
        critical,
    } = mode
    {
        if records.is_empty() {
            output::print_bar(
                None,
                template,
                warn,
                critical,
                bar_period_label(period),
                &exchange,
            );
            return Ok(());
        }

        let pricing = pricing::load_pricing(&pricing_source, cli.offline)?;
        let buckets = aggregate::aggregate(&records, &mode, &pricing);
        let bucket = buckets.values().next();
        output::print_bar(
            bucket,
            template,
            warn,
            critical,
            bar_period_label(period),
            &exchange,
        );
        return Ok(());
    }

    if records.is_empty() {
        eprintln!("No usage records found.");
        return Ok(());
    }

    eprintln!("Found {} usage records.", records.len());

    let pricing = pricing::load_pricing(&pricing_source, cli.offline)?;

    let unpriced = pricing.unpriced_models(&records);
    if !unpriced.is_empty() {
        eprintln!("No pricing data for: {}", unpriced.join(", "));
    }

    let buckets = aggregate::aggregate(&records, &mode, &pricing);

    let columns = cli::resolve_columns(cli.columns);

    match cli.format {
        cli::OutputFormat::Json => output::print_json(&buckets, &exchange),
        cli::OutputFormat::Table => {
            output::print_table(&buckets, &columns, cli.breakdown, &exchange)
        }
    }

    Ok(())
}
