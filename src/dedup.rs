use std::collections::HashMap;

use crate::types::{Provider, UsageRecord};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(mut h: u64, field: &[u8]) -> u64 {
    for &b in field.iter().chain(&[0xff]) {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Identity of a message across files and runs: FNV-1a over provider, message
/// id and request id, each followed by a `0xff` byte. Stable across Rust
/// releases, so it can be persisted.
pub(crate) fn stable_fingerprint(provider: Provider, message_id: &str, request_id: &str) -> u64 {
    let h = fnv1a(FNV_OFFSET, provider.as_str().as_bytes());
    let h = fnv1a(h, message_id.as_bytes());
    fnv1a(h, request_id.as_bytes())
}

/// In-memory dedup key: `fp` continued over the account, so one message seen
/// under two accounts is not collapsed into whichever was scanned first.
pub(crate) fn dedup_key(fp: u64, account_uuid: Option<&str>) -> u64 {
    fnv1a(fp, account_uuid.unwrap_or("").as_bytes())
}

fn record_key(r: &UsageRecord) -> u64 {
    dedup_key(
        stable_fingerprint(r.provider, &r.message_id, &r.request_id),
        r.account_uuid.as_deref(),
    )
}

/// The five token fields of one copy of a message, kept or replaced as a unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tokens {
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_creation_1h: u64,
    cache_read: u64,
}

impl Tokens {
    pub(crate) fn of(r: &UsageRecord) -> Self {
        Self {
            input: r.input_tokens,
            output: r.output_tokens,
            cache_creation: r.cache_creation_input_tokens,
            cache_creation_1h: r.cache_creation_1h_input_tokens,
            cache_read: r.cache_read_input_tokens,
        }
    }

    pub(crate) fn apply(self, r: &mut UsageRecord) {
        r.input_tokens = self.input;
        r.output_tokens = self.output;
        r.cache_creation_input_tokens = self.cache_creation;
        r.cache_creation_1h_input_tokens = self.cache_creation_1h;
        r.cache_read_input_tokens = self.cache_read;
    }

    /// Whether this copy is more complete than `other`: higher output, then a
    /// larger input plus cache total. Equal copies are not, so ties keep the first.
    pub(crate) fn beats(self, other: Self) -> bool {
        let context = |t: Self| {
            t.input
                .saturating_add(t.cache_creation)
                .saturating_add(t.cache_read)
        };
        (self.output, context(self)) > (other.output, context(other))
    }
}

/// Fold another copy of `kept`'s message into it: the more complete copy's
/// tokens (see [`Tokens::beats`]) and the earliest timestamp.
pub(crate) fn merge_copy(kept: &mut UsageRecord, copy: &UsageRecord) {
    let tokens = Tokens::of(copy);
    if tokens.beats(Tokens::of(kept)) {
        tokens.apply(kept);
    }
    kept.timestamp = kept.timestamp.min(copy.timestamp);
}

/// Merge records sharing a dedup key, in first-occurrence order.
///
/// Claude Code writes one line per content block, all with the same ids and
/// only the last carrying the final usage, so the token fields come as a unit
/// from the copy with the highest output (see [`merge_copy`]) and the timestamp
/// is the earliest; other fields come from the first copy. Aggregates and
/// records without any id pass through unmerged.
pub fn dedup(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let mut index: HashMap<u64, usize> = HashMap::with_capacity(records.len());
    let mut out: Vec<UsageRecord> = Vec::with_capacity(records.len());
    for r in records {
        if r.aggregate || (r.message_id.is_empty() && r.request_id.is_empty()) {
            out.push(r);
            continue;
        }
        match index.get(&record_key(&r)) {
            Some(&i) => merge_copy(&mut out[i], &r),
            None => {
                index.insert(record_key(&r), out.len());
                out.push(r);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn rec(provider: Provider, message_id: &str, request_id: &str) -> UsageRecord {
        UsageRecord {
            provider,
            session_id: "s".to_string(),
            timestamp: ts("2026-01-01T00:00:00Z"),
            project: "p".to_string(),
            model: "m".to_string(),
            message_id: message_id.to_string(),
            request_id: request_id.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: false,
            account_uuid: None,
            aggregate: false,
        }
    }

    fn out_tokens(provider: Provider, id: &str, output: u64) -> UsageRecord {
        UsageRecord {
            output_tokens: output,
            ..rec(provider, id, "r1")
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
        // Ordering must match first occurrence
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
    fn partial_output_counts_merge_to_the_maximum() {
        let out = dedup(vec![
            out_tokens(Provider::Claude, "m1", 6),
            out_tokens(Provider::Claude, "m1", 6),
            out_tokens(Provider::Claude, "m1", 410),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].output_tokens, 410);
    }

    #[test]
    fn merge_keeps_the_highest_output_copy_whole_and_the_earliest_timestamp() {
        let a = UsageRecord {
            timestamp: ts("2026-01-01T10:00:05Z"),
            input_tokens: 3,
            output_tokens: 1,
            cache_creation_input_tokens: 100,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 7,
            ..rec(Provider::Claude, "m1", "r1")
        };
        let b = UsageRecord {
            timestamp: ts("2026-01-01T10:00:01Z"),
            project: "other".to_string(),
            input_tokens: 2,
            output_tokens: 9,
            cache_creation_input_tokens: 50,
            cache_creation_1h_input_tokens: 40,
            cache_read_input_tokens: 8,
            ..rec(Provider::Claude, "m1", "r1")
        };
        let out = dedup(vec![a, b]);
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(
            (
                r.input_tokens,
                r.output_tokens,
                r.cache_creation_input_tokens,
                r.cache_creation_1h_input_tokens,
                r.cache_read_input_tokens
            ),
            (2, 9, 50, 40, 8)
        );
        assert_eq!(r.timestamp, ts("2026-01-01T10:00:01Z"));
        assert_eq!(r.project, "p");
    }

    fn split(input: u64, cache_read: u64, out: u64) -> UsageRecord {
        UsageRecord {
            input_tokens: input,
            output_tokens: out,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: cache_read,
            ..rec(Provider::Claude, "m1", "r1")
        }
    }

    fn fields(r: &UsageRecord) -> (u64, u64, u64) {
        (r.input_tokens, r.cache_read_input_tokens, r.output_tokens)
    }

    #[test]
    fn mixed_split_keeps_the_final_line_whole() {
        let out = dedup(vec![
            split(2, 107_292, 8),
            split(2, 107_292, 8),
            split(3535, 97_739, 2075),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(fields(&out[0]), (3535, 97_739, 2075));
    }

    #[test]
    fn equal_output_goes_to_the_larger_context_then_the_first_copy() {
        let out = dedup(vec![split(1, 10, 5), split(2, 20, 5), split(20, 2, 5)]);
        assert_eq!(fields(&out[0]), (2, 20, 5));
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert_eq!(dedup(Vec::new()).len(), 0);
    }

    #[test]
    fn same_msg_req_under_different_accounts_does_not_collapse() {
        // Collapsing here would silently transfer one account's tokens to the other.
        let mut a = out_tokens(Provider::Claude, "m1", 5);
        a.account_uuid = Some("org-aaa".to_string());
        let mut b = out_tokens(Provider::Claude, "m1", 9);
        b.account_uuid = Some("org-bbb".to_string());
        let out = dedup(vec![a, b]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].account_uuid.as_deref(), Some("org-aaa"));
        assert_eq!(out[0].output_tokens, 5);
        assert_eq!(out[1].account_uuid.as_deref(), Some("org-bbb"));
        assert_eq!(out[1].output_tokens, 9);
    }

    #[test]
    fn untagged_records_still_dedup_against_each_other() {
        let out = dedup(vec![
            rec(Provider::Claude, "m1", "r1"),
            rec(Provider::Claude, "m1", "r1"),
        ]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn keyless_records_are_never_merged() {
        let out = dedup(vec![
            UsageRecord {
                output_tokens: 6,
                ..rec(Provider::Claude, "", "")
            },
            UsageRecord {
                output_tokens: 410,
                ..rec(Provider::Claude, "", "")
            },
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].output_tokens, 6);
        assert_eq!(out[1].output_tokens, 410);
    }

    #[test]
    fn records_with_one_id_still_merge() {
        let out = dedup(vec![
            out_tokens(Provider::Claude, "m1", 1),
            UsageRecord {
                output_tokens: 4,
                ..rec(Provider::Claude, "", "r9")
            },
            UsageRecord {
                output_tokens: 8,
                ..rec(Provider::Claude, "", "r9")
            },
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].output_tokens, 8);
    }

    #[test]
    fn aggregates_with_equal_ids_are_both_kept() {
        let agg = |output| {
            let mut r = out_tokens(Provider::Claude, "", output);
            r.aggregate = true;
            r.message_id = crate::types::aggregate_id(&r);
            r.request_id = "tku-agg".to_string();
            r
        };
        let out = dedup(vec![agg(10), agg(20)]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].message_id, out[1].message_id);
        assert_eq!(out[0].output_tokens + out[1].output_tokens, 30);
    }

    #[test]
    fn fingerprint_is_pinned() {
        assert_eq!(
            stable_fingerprint(Provider::Claude, "msg_1", "req_1"),
            0x2ace_50ef_438b_d68f
        );
        let fp = stable_fingerprint(Provider::Claude, "msg_1", "req_1");
        assert_eq!(dedup_key(fp, Some("org-1")), 0xd501_84ef_b1be_a3ba);
    }

    #[test]
    fn field_boundaries_change_the_fingerprint() {
        assert_ne!(
            stable_fingerprint(Provider::Claude, "ab", "c"),
            stable_fingerprint(Provider::Claude, "a", "bc")
        );
        let fp = stable_fingerprint(Provider::Claude, "m", "r");
        assert_ne!(dedup_key(fp, None), fp);
    }
}
