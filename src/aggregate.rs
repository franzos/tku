use std::collections::{BTreeMap, HashMap, HashSet};

use crate::cli::Command;
use crate::cost::PricingMap;
use crate::types::{AggregatedBucket, ModelBucketDetail, UsageRecord};

/// Shorten model names for display: strip `claude-` prefix and date suffixes.
/// "claude-opus-4-6" → "opus-4-6"
/// "claude-sonnet-4-5-20250929" → "sonnet-4-5"
/// "claude-haiku-4-5-20251001" → "haiku-4-5"
pub fn short_model_name(model: &str) -> String {
    let s = model.strip_prefix("claude-").unwrap_or(model);
    // Strip trailing date suffix (8 digits preceded by -)
    if s.len() > 9
        && s.as_bytes()[s.len() - 9] == b'-'
        && s[s.len() - 8..].chars().all(|c| c.is_ascii_digit())
    {
        s[..s.len() - 9].to_string()
    } else {
        s.to_string()
    }
}

/// Bucket key for grouping records.
/// For daily/monthly: the date/month string.
/// For session: "project | session_id".
pub fn bucket_key(record: &UsageRecord, mode: &Command) -> String {
    match mode {
        Command::Daily => record.timestamp.format("%Y-%m-%d").to_string(),
        Command::Monthly => record.timestamp.format("%Y-%m").to_string(),
        Command::Session => format!("{} | {}", record.project, record.session_id),
        Command::Model => record.model.clone(),
        Command::Watch { .. } => "watch".to_string(),
        Command::Bar { .. } => "bar".to_string(),
    }
}

/// All per-key state accumulated during the hot loop, bundled to avoid
/// maintaining four separate maps keyed by the same string.
#[derive(Default)]
struct BucketState {
    bucket: AggregatedBucket,
    projects: HashSet<String>,
    tools: HashSet<String>,
    model_details: HashMap<String, ModelBucketDetail>,
}

pub fn aggregate(
    records: &[UsageRecord],
    mode: &Command,
    pricing: &dyn PricingMap,
) -> BTreeMap<String, AggregatedBucket> {
    let mut states: HashMap<String, BucketState> = HashMap::new();

    for r in records {
        let key = bucket_key(r, mode);
        let record_cost = pricing.cost_for_record(r);

        // Single entry lookup per record — no extra clones
        let state = states.entry(key).or_default();

        state.bucket.accumulate(
            r.input_tokens,
            r.output_tokens,
            r.cache_creation_input_tokens,
            r.cache_read_input_tokens,
            record_cost,
        );

        state.projects.insert(r.project.clone());
        state.tools.insert(r.provider.clone());

        // Per-model detail
        let detail = state
            .model_details
            .entry(r.model.clone())
            .or_insert_with(|| ModelBucketDetail {
                model: r.model.clone(),
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                cost: None,
            });
        detail.accumulate(
            r.input_tokens,
            r.output_tokens,
            r.cache_creation_input_tokens,
            r.cache_read_input_tokens,
            record_cost,
        );
    }

    // Flatten BucketState into AggregatedBucket
    states
        .into_iter()
        .map(|(key, state)| {
            let mut bucket = state.bucket;

            let mut details: Vec<ModelBucketDetail> = state.model_details.into_values().collect();
            details.sort_by(|a, b| {
                b.cost
                    .unwrap_or(0.0)
                    .partial_cmp(&a.cost.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            bucket.models = details.iter().map(|d| short_model_name(&d.model)).collect();
            bucket.details = details;
            bucket.projects = state.projects.into_iter().collect();
            bucket.tools = state.tools.into_iter().collect();

            (key, bucket)
        })
        .collect()
}
