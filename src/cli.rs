use chrono::NaiveDate;
use clap::{Parser, Subcommand, ValueEnum};

use crate::pricing::PricingSource;

#[derive(Parser, Debug)]
#[command(
    name = "tku",
    about = "Token usage and cost tracker for LLM coding tools"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Start date filter (YYYY-MM-DD)
    #[arg(long, global = true)]
    pub from: Option<NaiveDate>,

    /// End date filter (YYYY-MM-DD)
    #[arg(long, global = true)]
    pub to: Option<NaiveDate>,

    /// Output format: table (default), json
    #[arg(long, global = true, default_value = "table")]
    pub format: OutputFormat,

    /// Use cached pricing only, don't fetch
    #[arg(long, global = true)]
    pub offline: bool,

    /// Show per-model breakdown within each period
    #[arg(long, global = true)]
    pub breakdown: bool,

    /// Filter by project name (substring match)
    #[arg(long, global = true)]
    pub project: Option<String>,

    /// Filter by tool (e.g. claude, codex, pi, amp)
    #[arg(long, global = true)]
    pub tool: Option<String>,

    /// Columns to display (comma-separated).
    /// Use +col to add, -col to remove from defaults, or plain names to replace.
    /// Available: period,input,output,cache_write,cache_read,cost,models,tools,projects
    #[arg(long, global = true, value_delimiter = ',', allow_hyphen_values = true)]
    pub columns: Option<Vec<String>>,

    /// Pricing source: litellm, openrouter, llmprices
    #[arg(long, global = true)]
    pub pricing_source: Option<PricingSource>,

    /// Currency code (ISO 4217) for cost display, e.g. EUR, GBP
    #[arg(long, global = true)]
    pub currency: Option<String>,

    /// Suppress progress output (for scripting)
    #[arg(long, global = true)]
    pub cli: bool,
}

pub const DEFAULT_COLUMNS: &[&str] = &[
    "period",
    "input",
    "output",
    "cache_write",
    "cache_read",
    "cost",
    "models",
    "tools",
];

/// Resolve `--columns` into a final list.
/// - No flag → defaults
/// - All prefixed with +/- → modify defaults (e.g. `+projects,-cache_write`)
/// - Plain names → explicit replacement (e.g. `period,cost,models`)
pub fn resolve_columns(raw: Option<Vec<String>>) -> Vec<String> {
    let Some(raw) = raw else {
        return DEFAULT_COLUMNS.iter().map(|s| s.to_string()).collect();
    };

    let is_modifier = raw.iter().all(|c| c.starts_with('+') || c.starts_with('-'));

    if !is_modifier {
        return raw;
    }

    let mut cols: Vec<String> = DEFAULT_COLUMNS.iter().map(|s| s.to_string()).collect();
    for entry in &raw {
        if let Some(name) = entry.strip_prefix('+') {
            if !cols.iter().any(|c| c == name) {
                cols.push(name.to_string());
            }
        } else if let Some(name) = entry.strip_prefix('-') {
            cols.retain(|c| c != name);
        }
    }
    cols
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Aggregate by day (default)
    Daily,
    /// Aggregate by month
    Monthly,
    /// Aggregate by session
    Session,
    /// Aggregate by model
    Model,
    /// Live-updating cost monitor
    Watch {
        /// Show full table instead of compact summary line
        #[arg(long)]
        full: bool,
        /// Minimum seconds between refreshes (debounce)
        #[arg(long, default_value = "2")]
        interval: u64,
    },
    /// Show a bar chart of token usage over time
    Plot {
        /// Period: 1d, 1w, 1m (default: 1m)
        #[arg(default_value = "1m")]
        period: GraphPeriod,
        /// Use relative time window (last N hours/days from now)
        #[arg(long)]
        relative: bool,
    },
    /// Output JSON for status bars (waybar, i3bar, polybar)
    Bar {
        /// Timeframe to summarize
        #[arg(long, default_value = "today")]
        period: BarPeriod,
        /// Format string for the text field. Placeholders: {cost}, {input}, {output}, {models}, {projects}
        #[arg(long, default_value = "{cost}")]
        template: String,
        /// Cost threshold that sets class to "warning"
        #[arg(long)]
        warn: Option<f64>,
        /// Cost threshold that sets class to "critical"
        #[arg(long)]
        critical: Option<f64>,
    },
}

#[derive(ValueEnum, Debug, Clone, PartialEq)]
pub enum BarPeriod {
    Today,
    Week,
    Month,
}

#[derive(ValueEnum, Debug, Clone, PartialEq)]
#[value(rename_all = "verbatim")]
pub enum GraphPeriod {
    /// Last 24 hours (30-min buckets)
    #[value(name = "1d")]
    Day,
    /// Last 7 days (6-hour buckets)
    #[value(name = "1w")]
    Week,
    /// Last 30 days (1-day buckets)
    #[value(name = "1m")]
    Month,
}

#[derive(ValueEnum, Debug, Clone, PartialEq)]
pub enum OutputFormat {
    Table,
    Json,
}

impl Cli {
    pub fn effective_command(&self) -> Command {
        self.command.clone().unwrap_or(Command::Daily)
    }
}
