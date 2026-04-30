use std::collections::HashSet;

use crate::types::UsageRecord;

/// Per-token pricing for a model.
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_input_token_cost: Option<f64>,
    pub cache_creation_input_token_cost: Option<f64>,
}

/// Trait for looking up pricing by model name.
pub trait PricingMap {
    fn get(&self, model: &str) -> Option<&ModelPricing>;

    fn cost_for_record(&self, r: &UsageRecord) -> Option<f64> {
        let p = self.get(&r.model)?;
        let mut cost = 0.0;
        cost += r.input_tokens as f64 * p.input_cost_per_token;
        cost += r.output_tokens as f64 * p.output_cost_per_token;
        if let Some(cr) = p.cache_read_input_token_cost {
            cost += r.cache_read_input_tokens as f64 * cr;
        }
        if let Some(cc) = p.cache_creation_input_token_cost {
            cost += r.cache_creation_input_tokens as f64 * cc;
        }
        Some(cost)
    }

    /// Models that appeared in records but have no pricing.
    ///
    /// Borrow `&str` through the dedup + lookup steps so we only allocate
    /// once per genuinely-unpriced model, not once per distinct model seen.
    /// On warm runs the priced-model subset dominates and this keeps the
    /// allocation count tiny.
    fn unpriced_models(&self, records: &[UsageRecord]) -> Vec<String> {
        let distinct: HashSet<&str> = records.iter().map(|r| r.model.as_str()).collect();
        let mut models: Vec<String> = distinct
            .into_iter()
            .filter(|m| self.get(m).is_none())
            .map(str::to_string)
            .collect();
        models.sort();
        models
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    struct TestPricing(HashMap<String, ModelPricing>);
    impl PricingMap for TestPricing {
        fn get(&self, model: &str) -> Option<&ModelPricing> {
            self.0.get(model)
        }
    }

    fn rec(model: &str) -> UsageRecord {
        UsageRecord {
            provider: crate::types::Provider::Claude,
            session_id: "s".into(),
            timestamp: Utc::now(),
            project: "proj".into(),
            model: model.to_string(),
            message_id: "m".into(),
            request_id: "r".into(),
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            account_uuid: None,
        }
    }

    fn priced(name: &str) -> (String, ModelPricing) {
        (
            name.to_string(),
            ModelPricing {
                input_cost_per_token: 0.0,
                output_cost_per_token: 0.0,
                cache_read_input_token_cost: None,
                cache_creation_input_token_cost: None,
            },
        )
    }

    #[test]
    fn unpriced_is_sorted_and_deduped() {
        let mut map = HashMap::new();
        let (k, v) = priced("priced-a");
        map.insert(k, v);
        let pricing = TestPricing(map);

        let records = vec![
            rec("priced-a"),
            rec("zeta"),
            rec("alpha"),
            rec("alpha"), // dup of #3
            rec("priced-a"),
            rec("beta"),
        ];
        let out = pricing.unpriced_models(&records);
        assert_eq!(out, vec!["alpha".to_string(), "beta".into(), "zeta".into()]);
    }
}
