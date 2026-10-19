mod accounts;
mod aggregate;
mod atomic_write;
mod burn;
mod cli;
mod config;
mod cost;
mod creds;
mod dedup;
mod exchange;
mod graph;
mod http;
mod migrate;
mod output;
mod paths;
mod pricing;
mod progress;
mod providers;
mod scrub;
mod spawn;
mod storage;
mod subscription;
mod types;
mod watch;

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

/// Does this record belong to `account`? `None` matches every record.
fn matches_account(
    record: &types::UsageRecord,
    account: Option<&str>,
    registry: &accounts::Registry,
) -> bool {
    let Some(name) = account else {
        return true;
    };
    record.provider == Provider::Claude
        && registry.owner_of(record.account_uuid.as_deref(), record.timestamp) == Some(name)
}

fn handle_account(action: &cli::AccountAction) -> Result<()> {
    match action {
        cli::AccountAction::Add { name } => accounts::add(name),
        cli::AccountAction::Use { name } => accounts::use_account(name),
        cli::AccountAction::List => accounts::list(),
        cli::AccountAction::Current => accounts::current(),
        cli::AccountAction::Rename { old, new } => accounts::rename(old, new),
        cli::AccountAction::Remove { name, force } => accounts::remove(name, *force),
        cli::AccountAction::Exec {
            name,
            ephemeral,
            clean,
            copy,
            args,
        } => {
            let code = spawn::run(name, *ephemeral, *clean, *copy, args.clone())?;
            std::process::exit(code);
        }
    }
}

#[cfg(feature = "sqlite")]
fn handle_cache(action: &cli::CacheAction) -> Result<()> {
    let cli::CacheAction::ImportSqlite { db, dry_run } = action;
    let path = db
        .clone()
        .or_else(paths::sqlite_db_file)
        .ok_or_else(|| anyhow::anyhow!("could not resolve a sqlite cache path"))?;

    let report = storage::import::import_sqlite(&path, *dry_run)?;
    let verb = if *dry_run { "Would import" } else { "Imported" };
    println!(
        "{verb} {} records from {} session files in {}",
        report.records,
        report.files,
        path.display()
    );
    if let Some((from, to)) = report.range {
        println!("Covering {} to {}", from.date_naive(), to.date_naive());
    }
    if report.skipped_files > 0 {
        println!(
            "Left {} files alone — the bitcode cache already has them",
            report.skipped_files
        );
    }
    Ok(())
}

#[cfg(not(feature = "sqlite"))]
fn handle_cache(action: &cli::CacheAction) -> Result<()> {
    let cli::CacheAction::ImportSqlite { .. } = action;
    anyhow::bail!(
        "this build cannot read sqlite. Rebuild with the feature and run the import once:\n\
         \n    cargo build --release --features sqlite\n    \
         ./target/release/tku cache import-sqlite\n\n\
         It writes into the bitcode cache, so this build picks the records up afterwards."
    )
}

fn handle_scrub(mode: &cli::Command, format: &cli::OutputFormat, quiet: bool) -> Result<()> {
    let cli::Command::Scrub {
        only,
        skip,
        apply,
        stale_after,
        max_spread,
        include_wide,
        preview,
        export,
        allow,
        list_detectors,
    } = mode
    else {
        unreachable!("handle_scrub called with a non-scrub command");
    };

    let mut state = scrub::state::load_or_init()?;
    let custom = scrub::detect::build_custom(&state.custom)?;

    if *list_detectors {
        let mut table = comfy_table::Table::new();
        table.set_header(vec!["CATEGORY", "DETECTOR", "DEFAULT", "SOURCE"]);
        for d in scrub::detect::DETECTORS
            .iter()
            .chain(custom.iter().copied())
        {
            let builtin = scrub::detect::DETECTORS.iter().any(|b| b.id == d.id);
            table.add_row(vec![
                d.category.as_str(),
                d.id,
                if d.default_on { "on" } else { "off" },
                if builtin { "built-in" } else { "scrub.toml" },
            ]);
        }
        println!("{table}");
        return Ok(());
    }

    if let Some(fp) = allow {
        let fp = fp.trim().to_lowercase();
        if fp.len() < scrub::fingerprint::SHORT_LEN || !fp.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "expected a fingerprint from the report (at least {} hex characters), not a secret value",
                scrub::fingerprint::SHORT_LEN
            );
        }
        if state.allow_hash(&fp) {
            scrub::state::save(&state)?;
            println!("allowlisted {fp}");
        } else {
            println!("{fp} is already allowlisted");
        }
        return Ok(());
    }

    // 0 would make `age < Duration::ZERO` always false, disabling the
    // live-session guard entirely.
    let minutes = (*stale_after).max(1);
    if minutes != *stale_after {
        eprintln!("--stale-after raised to 1 minute; 0 would disable the live-session guard");
    }

    let opts = scrub::Options {
        only: only.clone(),
        skip: skip.clone(),
        apply: *apply,
        stale_after: std::time::Duration::from_secs(minutes * 60),
        max_spread: *max_spread,
        include_wide: *include_wide,
        preview: preview.unwrap_or(0),
        export: export.clone(),
    };
    // Progress goes to stderr, so it stays out of `--format json` on stdout.
    let progress = progress::Progress::new(!quiet);
    let progress_cb =
        |phase: &str, current: usize, total: usize| progress.count(phase, current, total);
    let outcome = scrub::run(
        &opts,
        &state,
        progress
            .is_active()
            .then_some(&progress_cb as scrub::Progress),
    )?;
    progress.clear();

    match format {
        cli::OutputFormat::Json => scrub::report::print_json(&outcome),
        cli::OutputFormat::Table => scrub::report::print_table(&outcome),
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mode = cli.effective_command();
    migrate::run_pending(!cli.cli);

    // Account management subcommands: handled early; no record scan needed.
    if let cli::Command::Account { action } = &mode {
        return handle_account(action);
    }

    if let cli::Command::Scrub { .. } = &mode {
        return handle_scrub(&mode, &cli.format, cli.cli);
    }

    if let cli::Command::Cache { action } = &mode {
        return handle_cache(action);
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

    // Reconcile vault with live creds: if Claude Code rotated the token
    // (background refresh, /login to the same account) since our last run,
    // mirror it into the active account's vault now. Otherwise the next
    // swap-back would restore a stale token and the user would 401.
    accounts::reconcile_live_creds(is_sub && !cli.offline);

    let mut store = storage::default_storage();

    let progress = progress::Progress::new(!cli.cli && !is_bar && !is_plot && !is_sub);
    // Throttle is applied inside providers::discover_and_parse_with so the
    // per-file hot path stays tight. This callback itself can be cheap.
    let progress_cb =
        |current: usize, total: usize| progress.count("Scanning sessions", current, total);
    for provider in providers::all_providers() {
        provider.discover_and_parse(
            store.as_mut(),
            progress
                .is_active()
                .then_some(&progress_cb as &dyn Fn(usize, usize)),
            cli.prune,
        );
    }
    progress.clear();

    store.flush();
    let all_records = store.drain_all();

    let records = dedup::dedup(all_records);

    let claude_refs: Vec<&types::UsageRecord> = records
        .iter()
        .filter(|r| r.provider == Provider::Claude)
        .collect();

    let account_filter = cli.account.clone();

    if let cli::Command::Subscription { live, plan, all } = mode {
        // `--all` shows every account; `--account <name>` scopes to one.
        // Combining them is contradictory, so reject the combination instead
        // of silently picking one (clap can't enforce this directly because
        // `--account` is global and `--all` is subcommand-local).
        if all && account_filter.is_some() {
            bail!("`--all` and `--account` are mutually exclusive");
        }
        if !cli.offline {
            accounts::bootstrap_via_profile_api(&claude_refs);
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

    drop(claude_refs);
    let account_registry = accounts::load_registry("claude");
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

    if let cli::Command::ModelBurn { idle_gap } = mode {
        let report = burn::compute(&records, &pricing, idle_gap);
        match cli.format {
            cli::OutputFormat::Json => output::print_burn_json(&report, &exchange),
            cli::OutputFormat::Table => output::print_burn_table(&report, &exchange),
        }
        return Ok(());
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

#[cfg(test)]
mod tests {
    use super::*;
    use accounts::test_support::{record, same_org_registry, ts};

    #[test]
    fn matches_account_separates_same_org_accounts() {
        let registry = same_org_registry();
        let a = record(Some("acct-a@org-1"), ts(5));
        let b = record(Some("acct-b@org-1"), ts(5));
        assert!(matches_account(&a, Some("alice"), &registry));
        assert!(!matches_account(&a, Some("bob"), &registry));
        assert!(matches_account(&b, Some("bob"), &registry));
        assert!(!matches_account(&b, Some("alice"), &registry));
    }

    #[test]
    fn matches_account_unknown_tag_falls_back_to_switch_log() {
        let registry = same_org_registry();
        let during_bob = record(Some("acct-gone"), ts(1));
        assert!(matches_account(&during_bob, Some("bob"), &registry));
        assert!(!matches_account(&during_bob, Some("alice"), &registry));
        let before_log = record(Some("acct-gone"), ts(-1));
        assert!(!matches_account(&before_log, Some("alice"), &registry));
        assert!(!matches_account(&before_log, Some("bob"), &registry));
    }
}
