use std::collections::BTreeMap;

use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, ContentArrangement, Table};

use crate::aggregate::short_model_name;
use crate::exchange::ExchangeRate;
use crate::types::AggregatedBucket;

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn column_header(col: &str) -> &str {
    match col {
        "period" => "Period",
        "input" => "Input",
        "output" => "Output",
        "cache_write" => "Cache Write",
        "cache_read" => "Cache Read",
        "cost" => "Cost",
        "models" => "Models",
        "tools" => "Tools",
        "projects" => "Projects",
        other => other,
    }
}

fn bucket_cell(col: &str, key: &str, bucket: &AggregatedBucket, exchange: &ExchangeRate) -> Cell {
    match col {
        "period" => Cell::new(key),
        "input" => Cell::new(format_tokens(bucket.input_tokens)),
        "output" => Cell::new(format_tokens(bucket.output_tokens)),
        "cache_write" => Cell::new(format_tokens(bucket.cache_creation_input_tokens)),
        "cache_read" => Cell::new(format_tokens(bucket.cache_read_input_tokens)),
        "cost" => Cell::new(exchange.format_cost(bucket.cost)),
        "models" => Cell::new(bucket.models.join(", ")),
        "tools" => Cell::new(bucket.tools.join(", ")),
        "projects" => Cell::new(bucket.projects.join(", ")),
        _ => Cell::new(""),
    }
}

fn detail_cell(
    col: &str,
    detail: &crate::types::ModelBucketDetail,
    exchange: &ExchangeRate,
) -> Cell {
    match col {
        "period" => Cell::new(format!("  {}", detail.model)),
        "input" => Cell::new(format_tokens(detail.input_tokens)),
        "output" => Cell::new(format_tokens(detail.output_tokens)),
        "cache_write" => Cell::new(format_tokens(detail.cache_creation_input_tokens)),
        "cache_read" => Cell::new(format_tokens(detail.cache_read_input_tokens)),
        "cost" => Cell::new(exchange.format_cost(detail.cost)),
        _ => Cell::new(""),
    }
}

pub fn print_table(
    buckets: &BTreeMap<String, AggregatedBucket>,
    columns: &[String],
    breakdown: bool,
    exchange: &ExchangeRate,
) {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_content_arrangement(ContentArrangement::Dynamic);

    table.set_header(columns.iter().map(|c| Cell::new(column_header(c))));

    let mut totals = AggregatedBucket::default();

    for (key, bucket) in buckets {
        table.add_row(
            columns
                .iter()
                .map(|c| bucket_cell(c, key, bucket, exchange)),
        );

        if breakdown {
            for detail in &bucket.details {
                table.add_row(columns.iter().map(|c| detail_cell(c, detail, exchange)));
            }
        }

        totals.accumulate_from(bucket);
    }

    table.add_row(
        columns
            .iter()
            .map(|c| bucket_cell(c, "TOTAL", &totals, exchange)),
    );

    println!("{table}");
}

pub fn print_bar(
    bucket: Option<&AggregatedBucket>,
    template: &str,
    warn: Option<f64>,
    critical: Option<f64>,
    period_label: &str,
    exchange: &ExchangeRate,
) {
    let Some(bucket) = bucket else {
        let zero = exchange.format_cost(Some(0.0));
        let output = serde_json::json!({
            "text": zero,
            "tooltip": "No usage",
            "class": "normal",
            "currency": exchange.code,
        });
        println!(
            "{}",
            serde_json::to_string(&output).expect("JSON serialization failed")
        );
        return;
    };

    let cost = bucket.cost.unwrap_or(0.0);
    let converted_cost = exchange.convert(cost);
    let cost_str = exchange.format_cost(Some(cost));

    let text = template
        .replace("{cost}", &cost_str)
        .replace("{input}", &format_tokens(bucket.input_tokens))
        .replace("{output}", &format_tokens(bucket.output_tokens))
        .replace("{models}", &bucket.models.join(", "))
        .replace("{projects}", &bucket.projects.join(", "));

    let mut tooltip = format!("{}: {}", period_label, cost_str);
    for detail in &bucket.details {
        let detail_cost = exchange.format_cost(detail.cost);
        tooltip.push_str(&format!(
            "\n  {}: {}",
            short_model_name(&detail.model),
            detail_cost
        ));
    }

    let class = if critical.is_some_and(|t| converted_cost >= t) {
        "critical"
    } else if warn.is_some_and(|t| converted_cost >= t) {
        "warning"
    } else {
        "normal"
    };

    let output = serde_json::json!({
        "text": text,
        "tooltip": tooltip,
        "class": class,
        "currency": exchange.code,
    });
    println!(
        "{}",
        serde_json::to_string(&output).expect("JSON serialization failed")
    );
}

pub fn print_json(buckets: &BTreeMap<String, AggregatedBucket>, exchange: &ExchangeRate) {
    let json: BTreeMap<&str, serde_json::Value> = buckets
        .iter()
        .map(|(key, bucket)| {
            let details: Vec<serde_json::Value> = bucket
                .details
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "model": d.model,
                        "input_tokens": d.input_tokens,
                        "output_tokens": d.output_tokens,
                        "cache_creation_input_tokens": d.cache_creation_input_tokens,
                        "cache_read_input_tokens": d.cache_read_input_tokens,
                        "cost": d.cost.map(|c| exchange.convert(c)),
                    })
                })
                .collect();

            (
                key.as_str(),
                serde_json::json!({
                    "currency": exchange.code,
                    "input_tokens": bucket.input_tokens,
                    "output_tokens": bucket.output_tokens,
                    "cache_creation_input_tokens": bucket.cache_creation_input_tokens,
                    "cache_read_input_tokens": bucket.cache_read_input_tokens,
                    "cost": bucket.cost.map(|c| exchange.convert(c)),
                    "models": bucket.models,
                    "projects": bucket.projects,
                    "details": details,
                }),
            )
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&json).expect("JSON serialization failed")
    );
}
