use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};

use super::bitcode_store::CachedFile;
use crate::dedup::{self, dedup_key, stable_fingerprint, Tokens};
use crate::types::{self, UsageRecord, AGG_MARKER};

/// The most complete copy's tokens and the earliest timestamp over every copy of one message.
struct Peak {
    tokens: Tokens,
    timestamp: DateTime<Utc>,
}

impl Peak {
    fn of(r: &UsageRecord) -> Self {
        Self {
            tokens: Tokens::of(r),
            timestamp: r.timestamp,
        }
    }

    fn raise(&mut self, r: &UsageRecord) {
        let tokens = Tokens::of(r);
        if tokens.beats(self.tokens) {
            self.tokens = tokens;
        }
        self.timestamp = self.timestamp.min(r.timestamp);
    }

    fn apply(&self, r: &mut UsageRecord) {
        self.tokens.apply(r);
        r.timestamp = self.timestamp;
    }
}

pub(crate) fn keyed(r: &UsageRecord) -> bool {
    !r.aggregate && (!r.message_id.is_empty() || !r.request_id.is_empty())
}

pub(crate) fn fingerprint(r: &UsageRecord) -> u64 {
    stable_fingerprint(r.provider, &r.message_id, &r.request_id)
}

fn merge_key(r: &UsageRecord) -> u64 {
    dedup_key(fingerprint(r), r.account_uuid.as_deref())
}

/// Pre-rollup entries of the files a [`rollup`] changed, oldest first; empty
/// when nothing changed.
pub(crate) type Undo = Vec<(String, CachedFile)>;

fn all_old_with_raw(cf: &CachedFile, cutoff: DateTime<Utc>) -> bool {
    cf.records.iter().any(|r| !r.aggregate) && cf.records.iter().all(|r| r.timestamp < cutoff)
}

/// A file is rolled once every raw record it holds is a keyed copy of a
/// message sealed in another file; anything else makes it a candidate.
fn is_candidate(key: &str, cf: &CachedFile, owners: &HashMap<u64, String>) -> bool {
    cf.records.iter().any(|r| {
        !r.aggregate && (!keyed(r) || owners.get(&fingerprint(r)).is_none_or(|o| o == key))
    })
}

/// Collapse every candidate file whose newest record predates `cutoff` to one
/// aggregate per (session, UTC day, project, model, account, fast mode),
/// recording the fingerprints it absorbed in `sealed`.
///
/// Candidates are visited in key order and each fingerprint is sealed in one
/// file only. Sealing is account-free, merging is not: an absorbed copy takes
/// the tokens of the most complete copy with the same fingerprint and account
/// (see [`dedup::merge_copy`]), and
/// once a message is sealed its copies under other accounts are not counted (a
/// replay is not new spend). A copy whose fingerprint is sealed in another
/// file stays raw and unsealed, so the read-time filter hides it while that
/// file holds the seal. Keyless records are summed into their own file's
/// aggregates and never sealed. Other files are read, not changed.
pub(crate) fn rollup(files: &mut HashMap<String, CachedFile>, cutoff: DateTime<Utc>) -> Undo {
    let mut candidates: Vec<String> = files
        .iter()
        .filter(|(_, cf)| all_old_with_raw(cf, cutoff))
        .map(|(k, _)| k.clone())
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let wanted: HashSet<u64> = candidates
        .iter()
        .flat_map(|k| &files[k].records)
        .filter(|r| keyed(r))
        .map(fingerprint)
        .collect();
    let mut owners: HashMap<u64, String> = HashMap::new();
    for (k, cf) in files.iter() {
        for fp in cf.sealed.iter().filter(|fp| wanted.contains(fp)) {
            owners.insert(*fp, k.clone());
        }
    }
    candidates.retain(|k| is_candidate(k, &files[k], &owners));
    if candidates.is_empty() {
        return Vec::new();
    }
    candidates.sort_unstable();
    let undo: Undo = candidates
        .iter()
        .map(|k| (k.clone(), files[k].clone()))
        .collect();

    let mut peaks: HashMap<u64, Peak> = HashMap::new();
    for key in &candidates {
        let Some(cf) = files.get_mut(key) else {
            continue;
        };
        cf.records = dedup::dedup(std::mem::take(&mut cf.records));
        for r in cf.records.iter().filter(|r| keyed(r)) {
            peaks.insert(merge_key(r), Peak::of(r));
        }
    }
    // Key order, so a tie between copies keeps the same one on every run.
    let mut keys: Vec<&String> = files.keys().collect();
    keys.sort_unstable();
    for r in keys
        .into_iter()
        .flat_map(|k| &files[k].records)
        .filter(|r| keyed(r))
    {
        if let Some(p) = peaks.get_mut(&merge_key(r)) {
            p.raise(r);
        }
    }

    for key in &candidates {
        let Some(cf) = files.get_mut(key) else {
            continue;
        };
        let mut sealed = std::mem::take(&mut cf.sealed);
        let own: HashSet<u64> = sealed.iter().copied().collect();
        let mut groups: BTreeMap<String, UsageRecord> = BTreeMap::new();
        let mut raw: Vec<UsageRecord> = Vec::new();
        for mut r in std::mem::take(&mut cf.records) {
            if keyed(&r) {
                let fp = fingerprint(&r);
                // Already counted in this file's aggregates.
                if own.contains(&fp) {
                    continue;
                }
                if owners.get(&fp).is_some_and(|o| o != key) {
                    raw.push(r);
                    continue;
                }
                if let Some(p) = peaks.get(&merge_key(&r)) {
                    p.apply(&mut r);
                }
                owners.insert(fp, key.clone());
                sealed.push(fp);
            }
            let id = types::aggregate_id(&r);
            match groups.get_mut(&id) {
                Some(agg) => {
                    agg.input_tokens += r.input_tokens;
                    agg.output_tokens += r.output_tokens;
                    agg.cache_creation_input_tokens += r.cache_creation_input_tokens;
                    agg.cache_creation_1h_input_tokens += r.cache_creation_1h_input_tokens;
                    agg.cache_read_input_tokens += r.cache_read_input_tokens;
                    agg.timestamp = agg.timestamp.min(r.timestamp);
                }
                None => {
                    r.aggregate = true;
                    r.message_id.clone_from(&id);
                    r.request_id = AGG_MARKER.to_string();
                    groups.insert(id, r);
                }
            }
        }
        sealed.sort_unstable();
        sealed.dedup();
        cf.records = groups.into_values().chain(raw).collect();
        cf.sealed = sealed;
    }
    undo
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::bitcode_store::ProviderCache;
    use crate::storage::format;
    use crate::types::Provider;

    fn rec(msg: &str, ts: &str, out: u64) -> UsageRecord {
        UsageRecord {
            provider: Provider::Claude,
            session_id: "s1".into(),
            timestamp: ts.parse().unwrap(),
            project: "p".into(),
            model: "opus".into(),
            message_id: msg.into(),
            request_id: format!("req-{msg}"),
            input_tokens: 1,
            output_tokens: out,
            cache_creation_input_tokens: 2,
            cache_creation_1h_input_tokens: 1,
            cache_read_input_tokens: 3,
            fast_mode: false,
            account_uuid: None,
            aggregate: false,
        }
    }

    fn keyless(ts: &str, out: u64) -> UsageRecord {
        UsageRecord {
            message_id: String::new(),
            request_id: String::new(),
            ..rec("", ts, out)
        }
    }

    fn file(records: Vec<UsageRecord>) -> CachedFile {
        CachedFile {
            mtime_secs: 0,
            size: 0,
            records,
            sealed: Vec::new(),
        }
    }

    fn files(entries: Vec<(&str, Vec<UsageRecord>)>) -> HashMap<String, CachedFile> {
        entries
            .into_iter()
            .map(|(k, rs)| (k.to_string(), file(rs)))
            .collect()
    }

    fn rolls(fs: &mut HashMap<String, CachedFile>, cutoff: DateTime<Utc>) -> bool {
        !rollup(fs, cutoff).is_empty()
    }

    fn aggregated(cf: &CachedFile) -> u64 {
        cf.records
            .iter()
            .filter(|r| r.aggregate)
            .map(|r| r.output_tokens)
            .sum()
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn fp(r: &UsageRecord) -> u64 {
        fingerprint(r)
    }

    fn output(cf: &CachedFile) -> u64 {
        cf.records.iter().map(|r| r.output_tokens).sum()
    }

    /// (day, project, model) -> output, over every file.
    fn daily(files: &HashMap<String, CachedFile>) -> BTreeMap<(String, String, String), u64> {
        let mut out = BTreeMap::new();
        for r in files.values().flat_map(|cf| &cf.records) {
            *out.entry((
                r.timestamp.date_naive().to_string(),
                r.project.clone(),
                r.model.clone(),
            ))
            .or_default() += r.output_tokens;
        }
        out
    }

    fn encode_ref(files: &HashMap<String, CachedFile>) -> Vec<u8> {
        encode(files.clone())
    }

    fn encode(files: HashMap<String, CachedFile>) -> Vec<u8> {
        format::encode(&ProviderCache {
            files,
            ..ProviderCache::default()
        })
    }

    #[test]
    fn daily_totals_are_kept_per_day_project_and_model() {
        let mut fs = files(vec![(
            "/a.jsonl",
            vec![
                rec("m1", "2020-01-01T01:00:00Z", 10),
                rec("m2", "2020-01-01T02:00:00Z", 5),
                UsageRecord {
                    project: "q".into(),
                    ..rec("m3", "2020-01-01T03:00:00Z", 4)
                },
                UsageRecord {
                    model: "sonnet".into(),
                    ..rec("m4", "2020-01-01T04:00:00Z", 3)
                },
                rec("m5", "2020-01-02T01:00:00Z", 7),
            ],
        )]);
        let before = daily(&fs);

        assert!(rolls(&mut fs, now()));

        assert_eq!(daily(&fs), before);
        let cf = &fs["/a.jsonl"];
        assert_eq!(cf.records.len(), 4);
        assert!(cf.records.iter().all(|r| r.aggregate
            && r.request_id == AGG_MARKER
            && r.message_id == types::aggregate_id(r)));
        let day1 = cf
            .records
            .iter()
            .find(|r| r.project == "p" && r.model == "opus" && r.output_tokens == 15)
            .unwrap();
        assert_eq!(day1.input_tokens, 2);
        assert_eq!(day1.cache_read_input_tokens, 6);
        assert_eq!(day1.timestamp.to_rfc3339(), "2020-01-01T01:00:00+00:00");
        assert_eq!(cf.sealed.len(), 5);
    }

    #[test]
    fn rollup_is_idempotent_and_byte_stable() {
        let build = |reverse: bool| {
            let mut entries = vec![
                (
                    "/a.jsonl",
                    vec![
                        rec("m1", "2020-01-01T01:00:00Z", 10),
                        rec("m2", "2020-01-02T01:00:00Z", 5),
                    ],
                ),
                (
                    "/b.jsonl",
                    vec![
                        rec("m2", "2020-01-02T01:00:00Z", 6),
                        rec("m3", "2020-01-03T01:00:00Z", 1),
                    ],
                ),
                ("/live.jsonl", vec![rec("m1", "2999-01-01T00:00:00Z", 12)]),
            ];
            if reverse {
                entries.reverse();
            }
            files(entries)
        };
        let mut once = build(false);
        assert!(rolls(&mut once, now()));
        let mut twice = build(true);
        assert!(rolls(&mut twice, now()));
        assert!(!rolls(&mut twice, now()));
        assert_eq!(encode(once), encode(twice));
    }

    #[test]
    fn a_fingerprint_is_sealed_in_one_file_only() {
        let shared = || rec("shared", "2020-01-01T01:00:00Z", 10);
        let mut fs = files(vec![
            (
                "/c.jsonl",
                vec![shared(), rec("c", "2020-01-01T02:00:00Z", 1)],
            ),
            ("/a.jsonl", vec![shared()]),
            (
                "/b.jsonl",
                vec![shared(), rec("b", "2020-01-01T02:00:00Z", 2)],
            ),
        ]);
        assert!(rolls(&mut fs, now()));

        let owners: Vec<&str> = fs
            .iter()
            .filter(|(_, cf)| cf.sealed.contains(&fp(&shared())))
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(owners, ["/a.jsonl"]);
        let mut all: Vec<u64> = fs.values().flat_map(|cf| cf.sealed.clone()).collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n);
        assert_eq!(fs.values().map(aggregated).sum::<u64>(), 13);
        for key in ["/b.jsonl", "/c.jsonl"] {
            let raw: Vec<&UsageRecord> = fs[key].records.iter().filter(|r| !r.aggregate).collect();
            assert_eq!(raw.len(), 1);
            assert_eq!(fp(raw[0]), fp(&shared()));
        }
        let before = encode_ref(&fs);
        assert!(!rolls(&mut fs, now()));
        assert_eq!(encode_ref(&fs), before);
    }

    #[test]
    fn an_existing_seal_wins_over_a_later_candidate() {
        let shared = rec("shared", "2020-01-01T01:00:00Z", 10);
        let mut fs = files(vec![
            ("/a.jsonl", vec![shared.clone()]),
            ("/z.jsonl", vec![shared.clone()]),
        ]);
        fs.get_mut("/z.jsonl").unwrap().sealed = vec![fp(&shared)];
        fs.get_mut("/z.jsonl").unwrap().records[0].aggregate = true;

        assert!(!rolls(&mut fs, now()));
        assert_eq!(fs["/a.jsonl"].records, [shared]);
        assert!(fs["/a.jsonl"].sealed.is_empty());
    }

    #[test]
    fn a_within_file_partial_duplicate_rolls_up_to_the_max() {
        let mut fs = files(vec![(
            "/a.jsonl",
            vec![
                rec("m1", "2020-01-01T01:00:00Z", 6),
                rec("m1", "2020-01-01T01:00:01Z", 6),
                rec("m1", "2020-01-01T01:00:02Z", 410),
            ],
        )]);
        assert!(rolls(&mut fs, now()));
        let cf = &fs["/a.jsonl"];
        assert_eq!(cf.records.len(), 1);
        assert_eq!(cf.records[0].output_tokens, 410);
        assert_eq!(
            cf.records[0].timestamp.to_rfc3339(),
            "2020-01-01T01:00:00+00:00"
        );
    }

    #[test]
    fn a_kept_copy_takes_the_most_complete_copy_in_other_files() {
        let mut fs = files(vec![
            ("/a.jsonl", vec![rec("m1", "2020-01-01T01:00:00Z", 6)]),
            (
                "/live.jsonl",
                vec![
                    rec("m1", "2020-01-01T01:00:00Z", 410),
                    rec("m9", "2999-01-01T00:00:00Z", 1),
                ],
            ),
        ]);
        assert!(rolls(&mut fs, now()));
        assert_eq!(output(&fs["/a.jsonl"]), 410);
        assert_eq!(fs["/live.jsonl"].records.len(), 2);
        assert!(fs["/live.jsonl"].sealed.is_empty());
    }

    #[test]
    fn mixed_split_across_files_keeps_the_final_line_whole() {
        let split = |input, cache_read, out| UsageRecord {
            input_tokens: input,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: cache_read,
            ..rec("m1", "2020-01-01T01:00:00Z", out)
        };
        let mut fs = files(vec![
            ("/a.jsonl", vec![split(2, 107_292, 8), split(2, 107_292, 8)]),
            ("/b.jsonl", vec![split(3535, 97_739, 2075)]),
            ("/live.jsonl", vec![rec("m9", "2999-01-01T00:00:00Z", 1)]),
        ]);
        assert!(rolls(&mut fs, now()));
        let agg = &fs["/a.jsonl"].records;
        assert_eq!(agg.len(), 1);
        assert_eq!(
            (
                agg[0].input_tokens,
                agg[0].cache_read_input_tokens,
                agg[0].output_tokens
            ),
            (3535, 97_739, 2075)
        );
    }

    #[test]
    fn keyless_records_are_kept_in_every_file() {
        let mut fs = files(vec![
            (
                "/a.jsonl",
                vec![
                    keyless("2020-01-01T01:00:00Z", 5),
                    keyless("2020-01-01T01:00:00Z", 5),
                ],
            ),
            ("/b.jsonl", vec![keyless("2020-01-01T01:00:00Z", 5)]),
        ]);
        assert!(rolls(&mut fs, now()));
        assert_eq!(output(&fs["/a.jsonl"]), 10);
        assert_eq!(output(&fs["/b.jsonl"]), 5);
        assert!(fs.values().all(|cf| cf.sealed.is_empty()));
    }

    /// Merging is per (fingerprint, account); sealing is per fingerprint.
    #[test]
    fn the_first_file_seals_a_message_for_every_account() {
        let tagged = |acct: &str, out| UsageRecord {
            account_uuid: Some(acct.into()),
            ..rec("m1", "2020-01-01T01:00:00Z", out)
        };
        let mut fs = files(vec![
            ("/a.jsonl", vec![tagged("org-a", 6), tagged("org-b", 7)]),
            ("/b.jsonl", vec![tagged("org-a", 50), tagged("org-c", 99)]),
        ]);
        assert!(rolls(&mut fs, now()));

        let a = &fs["/a.jsonl"];
        let by_account = |acct: &str| {
            a.records
                .iter()
                .find(|r| r.account_uuid.as_deref() == Some(acct))
                .map(|r| r.output_tokens)
        };
        assert_eq!(by_account("org-a"), Some(50));
        assert_eq!(by_account("org-b"), Some(7));
        assert_eq!(a.sealed, [fp(&tagged("org-a", 0))]);
        let b = &fs["/b.jsonl"];
        assert!(b.sealed.is_empty());
        assert_eq!(b.records.len(), 2);
        assert!(b.records.iter().all(|r| !r.aggregate));
    }

    #[test]
    fn a_raw_copy_of_a_message_sealed_in_its_own_file_is_dropped() {
        let m = rec("m1", "2020-01-01T01:00:00Z", 10);
        let mut fs = files(vec![("/a.jsonl", vec![m.clone()])]);
        assert!(rolls(&mut fs, now()));
        fs.get_mut("/a.jsonl").unwrap().records.push(m);

        assert!(rolls(&mut fs, now()));
        assert_eq!(output(&fs["/a.jsonl"]), 10);
        assert!(fs["/a.jsonl"].records.iter().all(|r| r.aggregate));
    }

    #[test]
    fn live_and_aggregate_only_files_are_not_candidates() {
        let mut fs = files(vec![(
            "/live.jsonl",
            vec![
                rec("m1", "2020-01-01T01:00:00Z", 1),
                rec("m2", "2999-01-01T00:00:00Z", 1),
            ],
        )]);
        assert!(!rolls(&mut fs, now()));
        assert_eq!(fs["/live.jsonl"].records.len(), 2);

        let mut old = files(vec![(
            "/old.jsonl",
            vec![rec("m1", "2020-01-01T01:00:00Z", 1)],
        )]);
        assert!(rolls(&mut old, now()));
        assert!(!rolls(&mut old, now()));
    }
}
