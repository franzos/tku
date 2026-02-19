use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::types::UsageRecord;

pub fn dedup(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let mut seen = HashSet::new();
    records
        .into_iter()
        .filter(|r| {
            let h = record_hash(r);
            seen.insert(h)
        })
        .collect()
}

fn record_hash(r: &UsageRecord) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    r.provider.hash(&mut hasher);
    r.message_id.hash(&mut hasher);
    r.request_id.hash(&mut hasher);
    hasher.finish()
}
