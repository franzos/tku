use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::types::{Provider, UsageRecord};

/// Fingerprint the three fields that together uniquely identify a record.
/// A `u64` key lets us drop the three-way String clone from the fast path —
/// same deterministic ordering, ~3× less allocation churn on large corpora.
///
/// `provider` hashes via its `as_str()` view so the key stays stable against
/// the String → enum migration: identical bytes in, identical hash out.
fn fingerprint(provider: Provider, message_id: &str, request_id: &str) -> u64 {
    let mut h = DefaultHasher::new();
    provider.as_str().hash(&mut h);
    message_id.hash(&mut h);
    request_id.hash(&mut h);
    h.finish()
}

pub fn dedup(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let mut seen: HashSet<u64> = HashSet::with_capacity(records.len());
    let mut out = Vec::with_capacity(records.len());
    for r in records {
        let key = fingerprint(r.provider, &r.message_id, &r.request_id);
        if seen.insert(key) {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rec(provider: Provider, message_id: &str, request_id: &str) -> UsageRecord {
        UsageRecord {
            provider,
            session_id: "s".to_string(),
            timestamp: Utc::now(),
            project: "p".to_string(),
            model: "m".to_string(),
            message_id: message_id.to_string(),
            request_id: request_id.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    #[test]
    fn identical_records_collapse_and_distinct_survive() {
        let records = vec![
            rec(Provider::Claude, "m1", "r1"),
            rec(Provider::Claude, "m1", "r1"), // dup of #1
            rec(Provider::Claude, "m2", "r1"), // distinct message
            rec(Provider::Codex, "m1", "r1"),  // distinct provider
            rec(Provider::Claude, "m1", "r2"), // distinct request
            rec(Provider::Claude, "m2", "r1"), // dup of #3
        ];
        let out = dedup(records);
        assert_eq!(out.len(), 4);
        // Ordering must match input (first occurrence wins)
        assert_eq!(
            (out[0].provider, out[0].message_id.as_str()),
            (Provider::Claude, "m1")
        );
        assert_eq!(
            (out[1].provider, out[1].message_id.as_str()),
            (Provider::Claude, "m2")
        );
        assert_eq!(
            (out[2].provider, out[2].message_id.as_str()),
            (Provider::Codex, "m1")
        );
        assert_eq!(
            (
                out[3].provider,
                out[3].message_id.as_str(),
                out[3].request_id.as_str()
            ),
            (Provider::Claude, "m1", "r2")
        );
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert_eq!(dedup(Vec::new()).len(), 0);
    }
}
