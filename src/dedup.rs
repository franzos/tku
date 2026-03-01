use std::collections::HashSet;

use crate::types::UsageRecord;

pub fn dedup(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let mut seen = HashSet::new();
    records
        .into_iter()
        .filter(|r| {
            let key = (&r.provider, &r.message_id, &r.request_id);
            seen.insert((key.0.clone(), key.1.clone(), key.2.clone()))
        })
        .collect()
}
