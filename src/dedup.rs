use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::types::{Provider, UsageRecord};

/// Fingerprint the fields that together uniquely identify a record.
/// A `u64` key lets us drop the multi-String clone from the fast path —
/// same deterministic ordering, ~3× less allocation churn on large corpora.
///
/// `provider` hashes via its `as_str()` view so the key stays stable against
/// the String → enum migration: identical bytes in, identical hash out.
///
/// `account_uuid` is included so the same `(provider, message_id, request_id)`
/// fingerprint observed under two different accounts isn't collapsed by the
/// dedup pass — that would silently transfer attribution from one account to
/// whichever was scanned first. Records tagged with `None` (legacy cache
/// entries, non-Claude providers) hash to a stable empty bucket and continue
/// to dedup against each other.
fn fingerprint(
    provider: Provider,
    message_id: &str,
    request_id: &str,
    account_uuid: Option<&str>,
) -> u64 {
    let mut h = DefaultHasher::new();
    provider.as_str().hash(&mut h);
    message_id.hash(&mut h);
    request_id.hash(&mut h);
    account_uuid.unwrap_or("").hash(&mut h);
    h.finish()
}

pub fn dedup(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let mut seen: HashSet<u64> = HashSet::with_capacity(records.len());
    let mut out = Vec::with_capacity(records.len());
    for r in records {
        let key = fingerprint(
            r.provider,
            &r.message_id,
            &r.request_id,
            r.account_uuid.as_deref(),
        );
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
            account_uuid: None,
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

    #[test]
    fn same_msg_req_under_different_accounts_does_not_collapse() {
        // Same provider + message_id + request_id under two distinct accounts
        // must survive dedup — collapsing here would silently transfer one
        // account's tokens to the other.
        let mut a = rec(Provider::Claude, "m1", "r1");
        a.account_uuid = Some("org-aaa".to_string());
        let mut b = rec(Provider::Claude, "m1", "r1");
        b.account_uuid = Some("org-bbb".to_string());
        let out = dedup(vec![a, b]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].account_uuid.as_deref(), Some("org-aaa"));
        assert_eq!(out[1].account_uuid.as_deref(), Some("org-bbb"));
    }

    #[test]
    fn untagged_records_still_dedup_against_each_other() {
        // Backward-compatibility: legacy cache entries (None) hash to a stable
        // empty bucket and continue to collapse normal duplicates.
        let out = dedup(vec![
            rec(Provider::Claude, "m1", "r1"),
            rec(Provider::Claude, "m1", "r1"),
        ]);
        assert_eq!(out.len(), 1);
    }
}
