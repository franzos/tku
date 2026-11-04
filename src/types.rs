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
#[cfg_attr(test, derive(PartialEq))]
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
    /// Portion of `cache_creation_input_tokens` written with a 1-hour TTL,
    /// which Anthropic prices at 2x base input against 1.25x for 5 minutes.
    /// The flat total stays authoritative; the 5-minute figure is derived as
    /// `total - this` at cost time so the two can never disagree.
    #[serde(default)]
    pub cache_creation_1h_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Fast mode, priced at a flat 2x on every token class.
    #[serde(default)]
    pub fast_mode: bool,
    /// Id of the registered Claude account that produced this record (see
    /// `accounts::Account::id`), assigned at scan time. None for
    /// non-Claude providers, for records cached before this field existed,
    /// or when credentials weren't readable during the scan. Filtering and
    /// per-account subscription views fall back to the timestamp-based
    /// switch log when this is None.
    #[serde(default)]
    pub account_uuid: Option<String>,
    /// A rollup of several messages, whose `message_id` is [`aggregate_id`].
    /// Skipped by serde so the legacy positional bitcode shape is unchanged.
    #[serde(skip)]
    pub aggregate: bool,
}

/// The `request_id` of every aggregate.
pub(crate) const AGG_MARKER: &str = "tku-agg";

/// Message id of the aggregate for `r`'s (session, UTC day, project, model,
/// account, fast mode).
pub fn aggregate_id(r: &UsageRecord) -> String {
    format!(
        "{AGG_MARKER}:{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        r.session_id,
        r.timestamp.date_naive(),
        r.project,
        r.model,
        r.account_uuid.as_deref().unwrap_or(""),
        r.fast_mode
    )
}

/// Flag a legacy `tku-agg:` record as an aggregate and give it its
/// [`aggregate_id`]. Returns whether `r` was one.
pub fn adopt_legacy_aggregate(r: &mut UsageRecord) -> bool {
    if r.aggregate || !r.message_id.starts_with("tku-agg:") {
        return false;
    }
    r.aggregate = true;
    r.message_id = aggregate_id(r);
    r.request_id = AGG_MARKER.to_string();
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: "s1".into(),
            timestamp: "2026-01-01T23:30:00Z".parse().unwrap(),
            project: "a:b".into(),
            model: "opus".into(),
            message_id: "msg".into(),
            request_id: "req".into(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: true,
            account_uuid: Some("org-1".into()),
            aggregate: false,
        }
    }

    #[test]
    fn aggregate_id_joins_the_key_fields() {
        assert_eq!(
            aggregate_id(&record()),
            "tku-agg:s1\u{1f}2026-01-01\u{1f}a:b\u{1f}opus\u{1f}org-1\u{1f}true"
        );
        let untagged = UsageRecord {
            account_uuid: None,
            fast_mode: false,
            ..record()
        };
        assert_eq!(
            aggregate_id(&untagged),
            "tku-agg:s1\u{1f}2026-01-01\u{1f}a:b\u{1f}opus\u{1f}\u{1f}false"
        );
    }

    #[test]
    fn a_legacy_aggregate_is_flagged_and_given_its_id() {
        let mut r = UsageRecord {
            message_id: "tku-agg:s1:2026-01-01:opus:org-1:true".into(),
            request_id: "tku-agg".into(),
            ..record()
        };
        assert!(adopt_legacy_aggregate(&mut r));
        assert!(r.aggregate);
        assert_eq!(r.message_id, aggregate_id(&r));
        assert_eq!(r.request_id, "tku-agg");
        assert!(!adopt_legacy_aggregate(&mut r));
    }

    #[test]
    fn a_raw_record_is_not_adopted() {
        let mut r = record();
        assert!(!adopt_legacy_aggregate(&mut r));
        assert!(!r.aggregate);
        assert_eq!(r.message_id, "msg");
    }
}
