use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::{EventKind, RecursiveMode, Watcher};

use crate::aggregate::short_model_name;
use crate::cli::{self, Command};
use crate::cost::PricingMap;
use crate::exchange::ExchangeRate;

pub fn run(
    mode: &Command,
    cli: &cli::Cli,
    pricing_source: &crate::pricing::PricingSource,
    currency: &str,
    date_range: Option<(chrono::NaiveDate, chrono::NaiveDate)>,
) -> Result<()> {
    let (full, interval) = match mode {
        Command::Watch { full, interval } => (*full, *interval),
        _ => unreachable!(),
    };

    let interval = Duration::from_secs(interval);
    let label = match date_range {
        Some((from, to)) if from == to => format!("{from}"),
        Some((from, to)) => format!("{from} — {to}"),
        None => "All time".to_string(),
    };

    // Load pricing once upfront (respects --offline on first fetch, then reused)
    let pricing = crate::pricing::load_pricing(pricing_source, cli.offline)?;

    // Initial render
    render(cli, &pricing, currency, date_range, full, &label)?;

    // Setup file watcher
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => {
                    let _ = tx.send(());
                }
                _ => {}
            }
        }
    })?;

    let watch_paths = crate::providers::all_watch_paths();
    if watch_paths.is_empty() {
        anyhow::bail!("No provider directories found to watch.");
    }

    for path in &watch_paths {
        watcher.watch(path, RecursiveMode::Recursive)?;
    }

    // Event loop with debounce
    while let Ok(()) = rx.recv() {
        // Debounce: drain any additional events within the interval
        let deadline = Instant::now() + interval;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }

        render(cli, &pricing, currency, date_range, full, &label)?;
    }

    Ok(())
}

fn scan_and_filter(
    cli: &cli::Cli,
    date_range: Option<(chrono::NaiveDate, chrono::NaiveDate)>,
) -> Vec<crate::types::UsageRecord> {
    let mut store = crate::storage::default_storage();

    for provider in crate::providers::all_providers() {
        provider.discover_and_parse(store.as_mut(), None);
    }

    store.flush();
    let all_records = store.drain_all();
    let records = crate::dedup::dedup(all_records);

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

    if let Some(ref tool) = cli.tool {
        let needle = tool.to_lowercase();
        records
            .into_iter()
            .filter(|r| r.provider.to_lowercase() == needle)
            .collect()
    } else {
        records
    }
}

fn render(
    cli: &cli::Cli,
    pricing: &dyn PricingMap,
    currency: &str,
    date_range: Option<(chrono::NaiveDate, chrono::NaiveDate)>,
    full: bool,
    label: &str,
) -> Result<()> {
    let records = scan_and_filter(cli, date_range);
    let exchange = crate::exchange::load_exchange_rate(currency, cli.offline);

    if full {
        render_full(&records, cli, pricing, &exchange)?;
    } else {
        render_compact(&records, pricing, &exchange, label)?;
    }

    Ok(())
}

fn render_compact(
    records: &[crate::types::UsageRecord],
    pricing: &dyn PricingMap,
    exchange: &ExchangeRate,
    label: &str,
) -> Result<()> {
    if records.is_empty() {
        eprint!("\x1b[2K\r{label}: {}", exchange.format_cost(Some(0.0)));
        std::io::stderr().flush()?;
        return Ok(());
    }

    let mode = Command::Watch {
        full: false,
        interval: 2,
    };
    let buckets = crate::aggregate::aggregate(records, &mode, pricing);

    let bucket = match buckets.values().next() {
        Some(b) => b,
        None => {
            eprint!("\x1b[2K\r{label}: {}", exchange.format_cost(Some(0.0)));
            std::io::stderr().flush()?;
            return Ok(());
        }
    };

    let total_cost = exchange.format_cost(bucket.cost);

    // Per-model breakdown
    let model_parts: Vec<String> = bucket
        .details
        .iter()
        .filter(|d| d.cost.is_some_and(|c| c > 0.0))
        .map(|d| {
            format!(
                "{}: {}",
                short_model_name(&d.model),
                exchange.format_cost(d.cost)
            )
        })
        .collect();

    let line = if model_parts.is_empty() {
        format!("{label}: {total_cost}")
    } else {
        format!("{label}: {total_cost} | {}", model_parts.join(", "))
    };

    eprint!("\x1b[2K\r{line}");
    std::io::stderr().flush()?;

    Ok(())
}

fn render_full(
    records: &[crate::types::UsageRecord],
    cli: &cli::Cli,
    pricing: &dyn PricingMap,
    exchange: &ExchangeRate,
) -> Result<()> {
    // Clear screen and move cursor to top-left
    print!("\x1b[2J\x1b[H");
    std::io::stdout().flush()?;

    if records.is_empty() {
        println!("No usage records found.");
        return Ok(());
    }

    // Use Daily aggregation for full table display
    let mode = Command::Daily;
    let buckets = crate::aggregate::aggregate(records, &mode, pricing);
    let columns = cli::resolve_columns(cli.columns.clone());

    crate::output::print_table(&buckets, &columns, cli.breakdown, exchange);

    Ok(())
}
