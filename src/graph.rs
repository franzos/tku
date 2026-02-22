use std::io::stdout;

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, Timelike};
use crossterm::execute;
use ratatui::{
    backend::CrosstermBackend,
    style::{Color, Style},
    widgets::{Bar, BarChart, BarGroup, Block},
    Terminal, TerminalOptions, Viewport,
};

use crate::cli::GraphPeriod;
use crate::types::UsageRecord;

struct BucketSpec {
    boundaries: Vec<DateTime<Local>>,
    labels: Vec<String>,
}

fn build_buckets(period: &GraphPeriod, relative: bool) -> BucketSpec {
    let now = Local::now();

    match period {
        GraphPeriod::Day => {
            let bucket_minutes = 30;
            let total_buckets = 48;
            let start = if relative {
                now - Duration::hours(24)
            } else {
                // Align to midnight yesterday, same time
                (now - Duration::days(1))
                    .with_minute(0)
                    .unwrap()
                    .with_second(0)
                    .unwrap()
                    .with_nanosecond(0)
                    .unwrap()
            };

            let mut boundaries = Vec::with_capacity(total_buckets + 1);
            let mut labels = Vec::with_capacity(total_buckets);

            for i in 0..=total_buckets {
                let t = start + Duration::minutes(i as i64 * bucket_minutes);
                if t > now {
                    break;
                }
                if i < total_buckets {
                    // Label on the hour only, skip :30 buckets
                    if t.minute() == 0 {
                        labels.push(format!("{:02}", t.hour()));
                    } else {
                        labels.push(String::new());
                    }
                }
                boundaries.push(t);
            }

            // Trim labels to match boundary pairs
            labels.truncate(boundaries.len().saturating_sub(1));

            BucketSpec { boundaries, labels }
        }
        GraphPeriod::Week => {
            let bucket_hours = 6;
            let total_buckets = 28;
            let start = if relative {
                now - Duration::days(7)
            } else {
                (now - Duration::days(7))
                    .with_hour(0)
                    .unwrap()
                    .with_minute(0)
                    .unwrap()
                    .with_second(0)
                    .unwrap()
                    .with_nanosecond(0)
                    .unwrap()
            };

            let mut boundaries = Vec::with_capacity(total_buckets + 1);
            let mut labels = Vec::with_capacity(total_buckets);
            let weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

            for i in 0..=total_buckets {
                let t = start + Duration::hours(i as i64 * bucket_hours);
                if t > now {
                    break;
                }
                if i < total_buckets {
                    // Label at midnight only with day abbreviation
                    if t.hour() == 0 {
                        let wd = weekdays[t.weekday().num_days_from_monday() as usize];
                        labels.push(wd.to_string());
                    } else {
                        labels.push(String::new());
                    }
                }
                boundaries.push(t);
            }

            labels.truncate(boundaries.len().saturating_sub(1));

            BucketSpec { boundaries, labels }
        }
        GraphPeriod::Month => {
            let total_buckets = 30;
            let start = if relative {
                now - Duration::days(30)
            } else {
                (now - Duration::days(29))
                    .with_hour(0)
                    .unwrap()
                    .with_minute(0)
                    .unwrap()
                    .with_second(0)
                    .unwrap()
                    .with_nanosecond(0)
                    .unwrap()
            };

            let mut boundaries = Vec::with_capacity(total_buckets + 1);
            let mut labels = Vec::with_capacity(total_buckets);
            let months = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];

            for i in 0..=total_buckets {
                let t = start + Duration::days(i as i64);
                if t > now {
                    break;
                }
                if i < total_buckets {
                    // Show day number; prepend month only on the 1st or first bucket
                    if t.day() == 1 || i == 0 {
                        let mon = months[t.month0() as usize];
                        labels.push(format!("{} {}", mon, t.day()));
                    } else {
                        labels.push(format!("{}", t.day()));
                    }
                }
                boundaries.push(t);
            }

            labels.truncate(boundaries.len().saturating_sub(1));

            BucketSpec { boundaries, labels }
        }
    }
}

fn total_tokens(r: &UsageRecord) -> u64 {
    r.input_tokens + r.output_tokens + r.cache_creation_input_tokens + r.cache_read_input_tokens
}

pub fn render(records: &[UsageRecord], period: &GraphPeriod, relative: bool) -> Result<()> {
    let spec = build_buckets(period, relative);
    let num_buckets = spec.labels.len();

    if num_buckets == 0 {
        eprintln!("No time buckets to display.");
        return Ok(());
    }

    // Bucket the records
    let mut values = vec![0u64; num_buckets];
    for r in records {
        let local_ts: DateTime<Local> = r.timestamp.with_timezone(&Local);
        // Binary search for the bucket
        let pos = spec
            .boundaries
            .partition_point(|b| *b <= local_ts)
            .saturating_sub(1);
        if pos < num_buckets {
            values[pos] += total_tokens(r);
        }
    }

    // Build bar data
    let bars: Vec<Bar> = spec
        .labels
        .iter()
        .zip(values.iter())
        .map(|(label, &val)| {
            Bar::default()
                .value(val)
                .label(label.clone().into())
                .style(Style::default().fg(Color::Cyan))
        })
        .collect();

    let title = match period {
        GraphPeriod::Day => "Token usage — last 24 hours (30-min buckets)",
        GraphPeriod::Week => "Token usage — last 7 days (6-hour buckets)",
        GraphPeriod::Month => "Token usage — last 30 days (daily buckets)",
    };

    let chart = BarChart::default()
        .block(Block::bordered().title(title))
        .data(BarGroup::default().bars(&bars))
        .bar_width(3)
        .bar_gap(1)
        .value_style(Style::default().fg(Color::White))
        .label_style(Style::default().fg(Color::DarkGray));

    let chart_height: u16 = 17; // 15 for bars + 2 for border

    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(chart_height),
        },
    )?;

    terminal.draw(|frame| {
        frame.render_widget(chart, frame.area());
    })?;

    // Move cursor below the chart
    execute!(stdout(), crossterm::cursor::MoveDown(1))?;

    Ok(())
}
