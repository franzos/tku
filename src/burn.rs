use std::collections::{HashMap, HashSet};

use chrono::Datelike;

use crate::cost::PricingMap;
use crate::types::UsageRecord;

pub struct ModelBurnRow {
    pub model: String,
    pub tokens: u64,
    pub cost: Option<f64>,
    pub active_secs: u64,
    #[allow(dead_code)]
    pub calendar_days: u64,
    pub tokens_per_min: Option<f64>,
    pub cost_per_active_hour: Option<f64>,
    pub cost_per_calendar_day: Option<f64>,
}

pub struct BurnReport {
    pub rows: Vec<ModelBurnRow>,
    pub total: ModelBurnRow,
}

fn record_tokens(r: &UsageRecord) -> u64 {
    r.input_tokens + r.output_tokens + r.cache_creation_input_tokens + r.cache_read_input_tokens
}

/// Sum of capped inter-record gaps within each group, summed across groups.
/// Records are grouped by an arbitrary key; the first record in a sorted
/// group contributes zero (no predecessor).
fn active_secs_by_group<K: std::hash::Hash + Eq>(
    records: &[&UsageRecord],
    cap_secs: i64,
    key: impl Fn(&UsageRecord) -> K,
) -> u64 {
    let mut groups: HashMap<K, Vec<i64>> = HashMap::new();
    for r in records {
        groups
            .entry(key(r))
            .or_default()
            .push(r.timestamp.timestamp());
    }

    let mut total: i64 = 0;
    for mut times in groups.into_values() {
        times.sort_unstable();
        for pair in times.windows(2) {
            let gap = (pair[1] - pair[0]).max(0);
            total += gap.min(cap_secs);
        }
    }
    total.max(0) as u64
}

fn distinct_local_days(records: &[&UsageRecord]) -> u64 {
    records
        .iter()
        .map(|r| {
            let d = r.timestamp.with_timezone(&chrono::Local).date_naive();
            (d.year(), d.ordinal())
        })
        .collect::<HashSet<_>>()
        .len() as u64
}

fn build_row<K: std::hash::Hash + Eq>(
    model: String,
    records: &[&UsageRecord],
    pricing: &dyn PricingMap,
    cap_secs: i64,
    group_key: impl Fn(&UsageRecord) -> K,
) -> ModelBurnRow {
    let tokens: u64 = records.iter().map(|r| record_tokens(r)).sum();

    // Any unpriced record poisons the whole row's cost to None, so the cost
    // column shows N/A rather than a silent undercount.
    let mut cost = Some(0.0_f64);
    for r in records {
        match (cost, pricing.cost_for_record(r)) {
            (Some(acc), Some(c)) => cost = Some(acc + c),
            _ => cost = None,
        }
    }

    let active_secs = active_secs_by_group(records, cap_secs, group_key);
    let calendar_days = distinct_local_days(records);

    let tokens_per_min = if active_secs > 0 {
        Some(tokens as f64 / (active_secs as f64 / 60.0))
    } else {
        None
    };
    let cost_per_active_hour = match (cost, active_secs > 0) {
        (Some(c), true) => Some(c / (active_secs as f64 / 3600.0)),
        _ => None,
    };
    let cost_per_calendar_day = match (cost, calendar_days > 0) {
        (Some(c), true) => Some(c / calendar_days as f64),
        _ => None,
    };

    ModelBurnRow {
        model,
        tokens,
        cost,
        active_secs,
        calendar_days,
        tokens_per_min,
        cost_per_active_hour,
        cost_per_calendar_day,
    }
}

pub fn compute(
    records: &[UsageRecord],
    pricing: &dyn PricingMap,
    idle_gap_mins: u64,
) -> BurnReport {
    let cap_secs = (idle_gap_mins * 60) as i64;

    let mut by_model: HashMap<&str, Vec<&UsageRecord>> = HashMap::new();
    for r in records {
        by_model.entry(r.model.as_str()).or_default().push(r);
    }

    // Per-model active time groups by (session, model); the ALL row groups by
    // session only. Per-model active times therefore won't sum to the ALL row.
    let mut rows: Vec<ModelBurnRow> = by_model
        .into_iter()
        .map(|(model, recs)| {
            build_row(model.to_string(), &recs, pricing, cap_secs, |r| {
                r.session_id.clone()
            })
        })
        .collect();

    rows.sort_by(|a, b| {
        match (a.cost, b.cost) {
            (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then(b.tokens.cmp(&a.tokens))
    });

    let all_refs: Vec<&UsageRecord> = records.iter().collect();
    let total = build_row("ALL".to_string(), &all_refs, pricing, cap_secs, |r| {
        r.session_id.clone()
    });

    BurnReport { rows, total }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::ModelPricing;
    use crate::types::Provider;
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;

    struct TestPricing(HashMap<String, ModelPricing>);
    impl PricingMap for TestPricing {
        fn get(&self, model: &str) -> Option<&ModelPricing> {
            self.0.get(model)
        }
    }

    fn pricing_for(models: &[&str]) -> TestPricing {
        let mut map = HashMap::new();
        for m in models {
            map.insert(
                (*m).to_string(),
                ModelPricing {
                    input_cost_per_token: 1.0,
                    output_cost_per_token: 0.0,
                    cache_read_input_token_cost: None,
                    cache_creation_input_token_cost: None,
                },
            );
        }
        TestPricing(map)
    }

    fn rec(session: &str, model: &str, ts: &str, input: u64) -> UsageRecord {
        let timestamp: DateTime<Utc> = DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .with_timezone(&Utc);
        UsageRecord {
            provider: Provider::Claude,
            session_id: session.to_string(),
            timestamp,
            project: "proj".into(),
            model: model.to_string(),
            message_id: "m".into(),
            request_id: "r".into(),
            input_tokens: input,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            account_uuid: None,
        }
    }

    fn row<'a>(report: &'a BurnReport, model: &str) -> &'a ModelBurnRow {
        report.rows.iter().find(|r| r.model == model).unwrap()
    }

    #[test]
    fn gap_capping() {
        let recs = vec![
            rec("s", "a", "2026-01-01T00:00:00Z", 1),
            rec("s", "a", "2026-01-01T00:02:00Z", 1),
            rec("s", "a", "2026-01-01T02:02:00Z", 1),
        ];
        let pricing = pricing_for(&["a"]);

        let report = compute(&recs, &pricing, 5);
        // 2min gap + (2h clamped to 5min) = 120 + 300 = 420s
        assert_eq!(row(&report, "a").active_secs, 420);

        let report = compute(&recs, &pricing, 9999);
        // full span: 0 → 2h02m = 7320s
        assert_eq!(row(&report, "a").active_secs, 7320);
    }

    #[test]
    fn single_isolated_record() {
        let recs = vec![rec("s", "a", "2026-01-01T00:00:00Z", 5)];
        let pricing = pricing_for(&["a"]);
        let report = compute(&recs, &pricing, 5);
        let r = row(&report, "a");
        assert_eq!(r.active_secs, 0);
        assert!(r.tokens_per_min.is_none());
        assert!(r.cost_per_active_hour.is_none());
    }

    #[test]
    fn interleaved_models() {
        // One session, A and B alternating one minute apart.
        let recs = vec![
            rec("s", "a", "2026-01-01T00:00:00Z", 1),
            rec("s", "b", "2026-01-01T00:01:00Z", 1),
            rec("s", "a", "2026-01-01T00:02:00Z", 1),
            rec("s", "b", "2026-01-01T00:03:00Z", 1),
        ];
        let pricing = pricing_for(&["a", "b"]);
        let report = compute(&recs, &pricing, 5);

        // Per-model: gaps between consecutive same-model records = 2min each.
        assert_eq!(row(&report, "a").active_secs, 120);
        assert_eq!(row(&report, "b").active_secs, 120);
        // ALL: session-wide consecutive gaps = 3 * 1min = 180s.
        assert_eq!(report.total.active_secs, 180);
        assert_ne!(
            row(&report, "a").active_secs + row(&report, "b").active_secs,
            report.total.active_secs
        );
    }

    #[test]
    fn distinct_calendar_days() {
        let recs = vec![
            rec("s", "a", "2026-01-01T12:00:00Z", 10),
            rec("s", "a", "2026-01-02T12:00:00Z", 10),
        ];
        let pricing = pricing_for(&["a"]);
        let report = compute(&recs, &pricing, 5);
        let r = row(&report, "a");
        assert_eq!(r.calendar_days, 2);
        let total_cost = r.cost.unwrap();
        assert!((r.cost_per_calendar_day.unwrap() - total_cost / 2.0).abs() < 1e-9);
    }

    #[test]
    fn unpriced_model() {
        let recs = vec![
            rec("s", "u", "2026-01-01T00:00:00Z", 7),
            rec("s", "u", "2026-01-01T00:01:00Z", 7),
        ];
        let pricing = pricing_for(&[]); // nothing priced
        let report = compute(&recs, &pricing, 5);
        let r = row(&report, "u");
        assert!(r.cost.is_none());
        assert!(r.cost_per_active_hour.is_none());
        assert!(r.cost_per_calendar_day.is_none());
        assert_eq!(r.tokens, 14);
        assert_eq!(r.active_secs, 60);
        assert!(r.tokens_per_min.is_some());
    }
}
