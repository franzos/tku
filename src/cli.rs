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

    /// Filter records to a specific account (requires prior `tku account add`).
    /// For now only Claude accounts are supported.
    #[arg(long, global = true)]
    pub account: Option<String>,

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

    /// Remove cached records for source files that no longer exist
    #[arg(long, global = true)]
    pub prune: bool,
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
    /// Show Claude Max/Pro subscription usage overview
    #[command(visible_alias = "sub")]
    Subscription {
        /// Force live API fetch instead of using estimated usage
        #[arg(long)]
        live: bool,
        /// Recommend whether to upgrade or downgrade your plan
        #[arg(long, conflicts_with = "all")]
        plan: bool,
        /// Show a compact one-row-per-account overview across all registered accounts
        #[arg(long)]
        all: bool,
    },
    /// Switch between multiple Claude logins and track usage per account
    #[command(
        long_about = "Switch between multiple Claude logins and track usage per account.

tku doesn't do the OAuth login itself — Claude Code does. `tku account` just
keeps a labeled copy of ~/.claude/.credentials.json for each login and swaps
the file in and out on demand.

Typical workflow for two accounts:

    # already logged into account A via Claude Code
    tku account add work

    # log into account B (Claude Code drives this, not tku)
    claude /logout
    claude /login

    tku account add personal

    # from now on:
    tku account use work
    tku account use personal"
    )]
    Account {
        #[command(subcommand)]
        action: AccountAction,
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

#[derive(Subcommand, Debug, Clone)]
pub enum AccountAction {
    /// Save your current Claude login as <name> so you can switch back to it later
    #[command(
        long_about = "Save your current Claude login as <name> so you can switch back to it later.

tku reads whatever credentials are currently in ~/.claude/.credentials.json
and stashes a copy. It cannot drive Claude Code's OAuth flow itself.

To register a *different* account, log out of Claude Code first and log back
in with the other one:

    claude /logout
    claude /login
    tku account add <other-name>"
    )]
    Add { name: String },
    /// Switch to a saved login (replaces ~/.claude/.credentials.json)
    #[command(long_about = "Switch the active Claude login to <name>.

Replaces ~/.claude/.credentials.json with the saved copy. Claude Code will
refresh the access token on next launch if needed — no re-login unless the
refresh token itself has expired.

Refuses by default if the current live login isn't saved (switching would
silently lose it). Pass --force to overwrite anyway.")]
    Use {
        name: String,
        /// Overwrite even when the current live login isn't saved
        #[arg(long, short)]
        force: bool,
    },
    /// List saved accounts and show which one is active
    List,
    /// Show which saved account is currently active
    Current,
    /// Rename a saved account
    Rename { old: String, new: String },
    /// Forget a saved account (deletes the stashed login; usage history is preserved)
    Remove {
        name: String,
        /// Allow removing the currently-active account
        #[arg(long, short)]
        force: bool,
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
