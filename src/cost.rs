use std::collections::HashSet;

use crate::types::UsageRecord;

/// Per-token pricing for a model.
#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_input_token_cost: Option<f64>,
    pub cache_creation_input_token_cost: Option<f64>,
    pub cache_creation_1h_input_token_cost: Option<f64>,
    pub supports_fast_mode: bool,
}

/// Anthropic's fast mode doubles every token class against the base rates.
const FAST_MODE_MULTIPLIER: f64 = 2.0;

/// Anthropic prices a 1-hour cache write at 2x base input against 1.25x for
/// five minutes. Sources that publish the 1-hour rate win; where one only
/// publishes the 5-minute rate the 2x ratio is exact across every Anthropic
/// model that carries the field, so it is derived rather than left 7% low.
/// A source with no 5-minute rate prices cache writes at zero and stays there.
pub fn resolve_cache_creation_1h_cost(
    input_cost_per_token: f64,
    cache_creation_cost: Option<f64>,
    explicit_1h_cost: Option<f64>,
) -> Option<f64> {
    match explicit_1h_cost {
        Some(c) => Some(c),
        None => cache_creation_cost.map(|_| input_cost_per_token * 2.0),
    }
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
        // The flat total is authoritative; the 5-minute figure is derived from
        // it so the two buckets can never disagree with the displayed tokens.
        let one_h = r
            .cache_creation_1h_input_tokens
            .min(r.cache_creation_input_tokens);
        let five_m = r.cache_creation_input_tokens - one_h;
        if let Some(cc) = p.cache_creation_input_token_cost {
            cost += five_m as f64 * cc;
        }
        if let Some(cc1h) = p.cache_creation_1h_input_token_cost {
            cost += one_h as f64 * cc1h;
        }
        if r.fast_mode && p.supports_fast_mode {
            cost *= FAST_MODE_MULTIPLIER;
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
            cache_creation_1h_input_tokens: 0,
            cache_read_input_tokens: 0,
            fast_mode: false,
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
                cache_creation_1h_input_token_cost: None,
                supports_fast_mode: false,
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

    /// Opus 5 list rates: $5/Mtok input, $6.25/Mtok 5-minute write,
    /// $10/Mtok 1-hour write.
    fn opus_5_pricing() -> ModelPricing {
        ModelPricing {
            input_cost_per_token: 5e-6,
            output_cost_per_token: 25e-6,
            cache_read_input_token_cost: Some(0.5e-6),
            cache_creation_input_token_cost: Some(6.25e-6),
            cache_creation_1h_input_token_cost: Some(10e-6),
            supports_fast_mode: true,
        }
    }

    fn map_of(model: &str, p: ModelPricing) -> TestPricing {
        let mut map = HashMap::new();
        map.insert(model.to_string(), p);
        TestPricing(map)
    }

    #[test]
    fn cache_writes_are_charged_per_ttl() {
        let pricing = map_of("claude-opus-5", opus_5_pricing());
        let mut r = rec("claude-opus-5");
        r.input_tokens = 0;
        r.output_tokens = 0;
        r.cache_creation_input_tokens = 1_000_000;
        r.cache_creation_1h_input_tokens = 400_000;

        // 600k x $6.25/M + 400k x $10/M
        let cost = pricing.cost_for_record(&r).unwrap();
        assert!((cost - (3.75 + 4.0)).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn a_one_hour_bucket_over_the_flat_total_is_clamped() {
        let pricing = map_of("claude-opus-5", opus_5_pricing());
        let mut r = rec("claude-opus-5");
        r.input_tokens = 0;
        r.output_tokens = 0;
        r.cache_creation_input_tokens = 1_000;
        r.cache_creation_1h_input_tokens = 5_000;

        let cost = pricing.cost_for_record(&r).unwrap();
        assert!((cost - 0.01).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn one_hour_rate_is_derived_as_twice_input_when_absent() {
        assert_eq!(
            resolve_cache_creation_1h_cost(5e-6, Some(6.25e-6), None),
            Some(10e-6)
        );
        assert_eq!(
            resolve_cache_creation_1h_cost(5e-6, Some(6.25e-6), Some(9e-6)),
            Some(9e-6)
        );
    }

    #[test]
    fn a_source_without_a_five_minute_rate_derives_nothing() {
        assert_eq!(resolve_cache_creation_1h_cost(5e-6, None, None), None);

        let mut p = opus_5_pricing();
        p.cache_creation_input_token_cost = None;
        p.cache_creation_1h_input_token_cost =
            resolve_cache_creation_1h_cost(p.input_cost_per_token, None, None);
        let pricing = map_of("claude-opus-5", p);

        let mut r = rec("claude-opus-5");
        r.input_tokens = 0;
        r.output_tokens = 0;
        r.cache_read_input_tokens = 0;
        r.cache_creation_input_tokens = 1_000_000;
        r.cache_creation_1h_input_tokens = 1_000_000;

        assert_eq!(pricing.cost_for_record(&r), Some(0.0));
    }

    #[test]
    fn fast_mode_doubles_a_record_on_a_model_that_supports_it() {
        let mut r = rec("claude-opus-5");
        r.input_tokens = 1_000_000;
        r.output_tokens = 1_000_000;
        r.cache_creation_input_tokens = 1_000_000;
        r.cache_creation_1h_input_tokens = 1_000_000;
        r.cache_read_input_tokens = 1_000_000;

        let standard = map_of("claude-opus-5", opus_5_pricing())
            .cost_for_record(&r)
            .unwrap();

        r.fast_mode = true;
        let fast = map_of("claude-opus-5", opus_5_pricing())
            .cost_for_record(&r)
            .unwrap();

        assert!(
            (standard - (5.0 + 25.0 + 10.0 + 0.5)).abs() < 1e-9,
            "{standard}"
        );
        assert!((fast - standard * 2.0).abs() < 1e-9, "{fast} vs {standard}");
    }

    #[test]
    fn fast_mode_is_ignored_on_a_model_without_it() {
        let mut p = opus_5_pricing();
        p.supports_fast_mode = false;
        let pricing = map_of("claude-sonnet-5", p);

        let mut r = rec("claude-sonnet-5");
        r.input_tokens = 1_000_000;
        r.output_tokens = 0;
        r.fast_mode = true;

        assert!((pricing.cost_for_record(&r).unwrap() - 5.0).abs() < 1e-9);
    }
}
