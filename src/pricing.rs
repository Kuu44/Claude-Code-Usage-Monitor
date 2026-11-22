//! List prices for the hover cost estimate, in US dollars per million tokens.
//!
//! Anthropic first-party list prices as of 2026-11-03. Cache writes use the
//! standard multipliers (5-minute 1.25x input, 1-hour 2x input) and cache reads
//! 0.1x input unless a model lists its own read price. Subscription plans are
//! not billed per token, so the hover labels this figure "est.".

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

impl ModelPrice {
    const fn new(input: f64, output: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
        }
    }

    pub fn cache_write_5m(self) -> f64 {
        self.input * 1.25
    }

    pub fn cache_write_1h(self) -> f64 {
        self.input * 2.0
    }
}

/// Most specific pattern first: the first substring found in the model id wins.
const CLAUDE_PRICES: &[(&str, ModelPrice)] = &[
    ("fable-5-2", ModelPrice::new(8.0, 40.0, 0.20)),
    ("mythos-5-2", ModelPrice::new(8.0, 40.0, 0.20)),
    ("fable", ModelPrice::new(10.0, 50.0, 1.00)),
    ("mythos", ModelPrice::new(10.0, 50.0, 1.00)),
    ("opus-5-5", ModelPrice::new(4.0, 20.0, 0.20)),
    ("opus-4-1", ModelPrice::new(15.0, 75.0, 1.50)),
    ("opus-4-2025", ModelPrice::new(15.0, 75.0, 1.50)),
    ("opus", ModelPrice::new(5.0, 25.0, 0.50)),
    ("sonnet-5-5", ModelPrice::new(2.0, 10.0, 0.10)),
    ("sonnet-5", ModelPrice::new(2.0, 10.0, 0.20)),
    ("sonnet", ModelPrice::new(3.0, 15.0, 0.30)),
    ("haiku-5-5", ModelPrice::new(0.10, 0.50, 0.01)),
    ("haiku", ModelPrice::new(1.0, 5.0, 0.10)),
];

pub fn claude_price(model: &str) -> Option<ModelPrice> {
    let model = model.to_ascii_lowercase();
    CLAUDE_PRICES
        .iter()
        .find(|(pattern, _)| model.contains(pattern))
        .map(|(_, price)| *price)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_matches_most_specific_model_first() {
        assert_eq!(claude_price("claude-opus-5-5").unwrap().input, 4.0);
        assert_eq!(claude_price("claude-opus-4-8").unwrap().input, 5.0);
        assert_eq!(claude_price("claude-sonnet-5-5").unwrap().cache_read, 0.10);
        assert_eq!(claude_price("claude-sonnet-5").unwrap().cache_read, 0.20);
        assert_eq!(claude_price("claude-sonnet-4-6").unwrap().input, 3.0);
        assert_eq!(claude_price("claude-haiku-5-5").unwrap().output, 0.50);
        assert_eq!(claude_price("claude-fable-5-2").unwrap().input, 8.0);
        assert!(claude_price("<synthetic>").is_none());
    }
}
