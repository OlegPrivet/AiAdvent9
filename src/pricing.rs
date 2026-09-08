use std::collections::HashMap;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;

use crate::metrics::TokenUsage;

const PRICES_URL: &str = "https://neuraldeep.ru/api/public/wallet-prices";
const PRICES_TIMEOUT: Duration = Duration::from_secs(5);
const PRICES_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Default)]
pub(crate) struct PriceCatalog {
    prices: HashMap<String, TokenPrice>,
    fetched_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CostEstimate {
    pub(crate) microrubles: u64,
    pub(crate) premium: bool,
}

#[derive(Debug, Error)]
pub(crate) enum PricingError {
    #[error("не удалось загрузить прайс: {0}")]
    Request(reqwest::Error),
}

impl PriceCatalog {
    pub(crate) async fn fetch() -> Result<Self, PricingError> {
        let client = Client::builder()
            .timeout(PRICES_TIMEOUT)
            .build()
            .map_err(PricingError::Request)?;
        let response = client
            .get(PRICES_URL)
            .send()
            .await
            .map_err(PricingError::Request)?
            .error_for_status()
            .map_err(PricingError::Request)?
            .json::<PricesResponse>()
            .await
            .map_err(PricingError::Request)?;
        let prices = response
            .prices
            .into_iter()
            .filter(|price| price.billing == "token")
            .map(|price| {
                let model = price.model.clone();
                (model, price.into())
            })
            .collect();
        Ok(Self {
            prices,
            fetched_at: Some(Instant::now()),
        })
    }

    pub(crate) fn is_stale(&self) -> bool {
        self.fetched_at
            .is_none_or(|fetched_at| fetched_at.elapsed() >= PRICES_TTL)
    }

    pub(crate) fn estimate(&self, model: &str, usage: TokenUsage) -> Option<CostEstimate> {
        let price = self.prices.get(model)?;
        let cached = usage.cached_prompt_tokens.min(usage.prompt_tokens);
        let regular = usage.prompt_tokens - cached;
        let input = regular as f64 * price.input_rub_per_million;
        let cached_input = cached as f64 * price.input_rub_per_million * 0.1;
        let output = usage.completion_tokens as f64 * price.output_rub_per_million;
        let microrubles = (input + cached_input + output).round();
        if !microrubles.is_finite() || microrubles.is_sign_negative() {
            return None;
        }
        Some(CostEstimate {
            microrubles: microrubles.min(u64::MAX as f64) as u64,
            premium: price.premium,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_price(model: &str, input: f64, output: f64, premium: bool) -> Self {
        Self {
            prices: HashMap::from([(
                model.to_owned(),
                TokenPrice {
                    input_rub_per_million: input,
                    output_rub_per_million: output,
                    premium,
                },
            )]),
            fetched_at: Some(Instant::now()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_prices(prices: &[(&str, f64, f64)]) -> Self {
        let mut catalog = Self::default();
        for (model, input, output) in prices {
            catalog
                .prices
                .extend(Self::with_price(model, *input, *output, false).prices);
        }
        catalog
    }
}

#[derive(Debug, Deserialize)]
struct PricesResponse {
    prices: Vec<PriceResponse>,
}

#[derive(Debug, Deserialize)]
struct PriceResponse {
    model: String,
    billing: String,
    in_rub_1m: f64,
    out_rub_1m: f64,
    premium: bool,
}

#[derive(Debug, Clone, Copy)]
struct TokenPrice {
    input_rub_per_million: f64,
    output_rub_per_million: f64,
    premium: bool,
}

impl From<PriceResponse> for TokenPrice {
    fn from(price: PriceResponse) -> Self {
        Self {
            input_rub_per_million: price.in_rub_1m,
            output_rub_per_million: price.out_rub_1m,
            premium: price.premium,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_regular_cached_and_output_tokens() {
        let catalog = PriceCatalog::with_price("paid", 10.0, 40.0, true);
        let usage = TokenUsage {
            prompt_tokens: 1_000,
            completion_tokens: 250,
            total_tokens: 1_250,
            cached_prompt_tokens: 400,
        };

        assert_eq!(
            catalog.estimate("paid", usage),
            Some(CostEstimate {
                microrubles: 16_400,
                premium: true,
            })
        );
    }

    #[test]
    fn returns_none_for_unknown_model() {
        assert_eq!(
            PriceCatalog::default().estimate("unknown", TokenUsage::default()),
            None
        );
    }
}
