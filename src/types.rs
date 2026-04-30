use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which LLM tool a [`UsageRecord`] came from.
///
/// Stored on disk as a lowercase string (via `serde(rename_all = "lowercase")`)
/// so JSON output and the sqlite `provider` TEXT column stay byte-identical
/// to the previous `provider: String` representation. The bitcode cache
/// re-serializes through the same serde path.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
    Pi,
    Amp,
    OpenCode,
    OpenClaw,
    Droid,
    Kimi,
}

impl Provider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Pi => "pi",
            Self::Amp => "amp",
            Self::OpenCode => "opencode",
            Self::OpenClaw => "openclaw",
            Self::Droid => "droid",
            Self::Kimi => "kimi",
        }
    }

    #[allow(dead_code)]
    pub fn iter() -> impl Iterator<Item = Self> {
        [
            Self::Claude,
            Self::Codex,
            Self::Gemini,
            Self::Pi,
            Self::Amp,
            Self::OpenCode,
            Self::OpenClaw,
            Self::Droid,
            Self::Kimi,
        ]
        .into_iter()
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "pi" => Ok(Self::Pi),
            "amp" => Ok(Self::Amp),
            "opencode" => Ok(Self::OpenCode),
            "openclaw" => Ok(Self::OpenClaw),
            "droid" => Ok(Self::Droid),
            "kimi" => Ok(Self::Kimi),
            _ => anyhow::bail!("unknown provider: {s}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub provider: Provider,
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
    /// Organization UUID of the Claude account that produced this record,
    /// captured at scan time from `~/.claude/.credentials.json`. None for
    /// non-Claude providers, for records cached before this field existed,
    /// or when credentials weren't readable during the scan. Filtering and
    /// per-account subscription views fall back to the timestamp-based
    /// switch log when this is None.
    #[serde(default)]
    pub account_uuid: Option<String>,
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
