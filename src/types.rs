use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub provider: String,
    pub session_id: String,
    pub timestamp: DateTime<Utc>,
    pub project: String,
    pub model: String,
    pub message_id: String,
    pub request_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct AggregatedBucket {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost: Option<f64>,
    pub models: Vec<String>,
    pub projects: Vec<String>,
    pub tools: Vec<String>,
    pub details: Vec<ModelBucketDetail>,
}

/// Merge an optional cost into an existing optional accumulator.
fn merge_cost(target: &mut Option<f64>, source: Option<f64>) {
    match (target, source) {
        (Some(ref mut c), Some(v)) => *c += v,
        (t @ None, Some(v)) => *t = Some(v),
        _ => {}
    }
}

impl AggregatedBucket {
    /// Accumulate token counts and cost from individual field values.
    /// Used by both the aggregation loop and the total-row computation.
    pub fn accumulate(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
        cost: Option<f64>,
    ) {
        self.input_tokens += input_tokens;
        self.output_tokens += output_tokens;
        self.cache_creation_input_tokens += cache_creation_input_tokens;
        self.cache_read_input_tokens += cache_read_input_tokens;
        merge_cost(&mut self.cost, cost);
    }

    /// Accumulate all token counts and cost from another bucket.
    pub fn accumulate_from(&mut self, other: &AggregatedBucket) {
        self.accumulate(
            other.input_tokens,
            other.output_tokens,
            other.cache_creation_input_tokens,
            other.cache_read_input_tokens,
            other.cost,
        );
    }
}

#[derive(Debug, Clone)]
pub struct ModelBucketDetail {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cost: Option<f64>,
}

impl ModelBucketDetail {
    /// Accumulate token counts and cost into this model detail.
    pub fn accumulate(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
        cost: Option<f64>,
    ) {
        self.input_tokens += input_tokens;
        self.output_tokens += output_tokens;
        self.cache_creation_input_tokens += cache_creation_input_tokens;
        self.cache_read_input_tokens += cache_read_input_tokens;
        merge_cost(&mut self.cost, cost);
    }
}
