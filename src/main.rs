mod aggregate;
mod cli;
mod cost;
mod dedup;
mod output;
mod pricing;
mod providers;
mod storage;
mod types;

use std::io::Write;

use anyhow::Result;
use chrono::Datelike;
use clap::Parser;

use cli::Cli;
use cost::PricingMap;

fn bar_date_range(period: &cli::BarPeriod) -> (chrono::NaiveDate, chrono::NaiveDate) {
    let today = chrono::Local::now().date_naive();
    match period {
        cli::BarPeriod::Today => (today, today),
        cli::BarPeriod::Week => (today - chrono::Duration::days(6), today),
        cli::BarPeriod::Month => {
            let first = chrono::NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mode = cli.effective_command();

    let is_bar = matches!(mode, cli::Command::Bar { .. });

    let date_range = if let cli::Command::Bar { ref period, .. } = mode {
        Some(bar_date_range(period))
    } else {
        match (cli.from, cli.to) {
            (Some(f), Some(t)) => Some((f, t)),
            (Some(f), None) => Some((f, chrono::Utc::now().date_naive())),
            (None, Some(t)) => Some((chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(), t)),
            (None, None) => None,
        }
    };

    let mut store = storage::default_storage();

    let show_progress = !cli.cli && !is_bar;
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
        );
    }
    if show_progress {
        eprint!("\x1b[2K\r");
        let _ = std::io::stderr().flush();
    }

    store.flush();
    let all_records = store.drain_all();

    let records = dedup::dedup(all_records);

    let records: Vec<_> = if let Some((from, to)) = date_range {
        records
            .into_iter()
            .filter(|r| {
                let date = r.timestamp.date_naive();
                date >= from && date <= to
            })
            .collect()
    } else {
        records
    };

    let records: Vec<_> = if let Some(ref proj) = cli.project {
        let needle = proj.to_lowercase();
        records
            .into_iter()
            .filter(|r| r.project.to_lowercase().contains(&needle))
            .collect()
    } else {
        records
    };

    let records: Vec<_> = if let Some(ref tool) = cli.tool {
        let needle = tool.to_lowercase();
        records
            .into_iter()
            .filter(|r| r.provider.to_lowercase() == needle)
            .collect()
    } else {
        records
    };

    if let cli::Command::Bar {
        ref period,
        ref template,
        warn,
        critical,
    } = mode
    {
        if records.is_empty() {
            output::print_bar(None, template, warn, critical, bar_period_label(period));
            return Ok(());
        }

        let pricing = pricing::load_pricing(cli.offline)?;
        let buckets = aggregate::aggregate(&records, &mode, &pricing);
        let bucket = buckets.values().next();
        output::print_bar(bucket, template, warn, critical, bar_period_label(period));
        return Ok(());
    }

    if records.is_empty() {
        eprintln!("No usage records found.");
        return Ok(());
    }

    eprintln!("Found {} usage records.", records.len());

    let pricing = pricing::load_pricing(cli.offline)?;

    let unpriced = pricing.unpriced_models(&records);
    if !unpriced.is_empty() {
        eprintln!("No pricing data for: {}", unpriced.join(", "));
    }

    let buckets = aggregate::aggregate(&records, &mode, &pricing);

    let columns = cli::resolve_columns(cli.columns);

    match cli.format {
        cli::OutputFormat::Json => output::print_json(&buckets),
        cli::OutputFormat::Table => output::print_table(&buckets, &columns, cli.breakdown),
    }

    Ok(())
}
