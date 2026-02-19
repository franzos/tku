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
    fn unpriced_models(&self, records: &[UsageRecord]) -> Vec<String> {
        let mut models: Vec<String> = records
            .iter()
            .map(|r| r.model.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .filter(|m| self.get(m).is_none())
            .collect();
        models.sort();
        models
    }
}
