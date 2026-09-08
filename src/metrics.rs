use serde::{Deserialize, Serialize};

use crate::pricing::PriceCatalog;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenUsage {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) total_tokens: u64,
    #[serde(default)]
    pub(crate) cached_prompt_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CallUsage {
    pub(crate) model: String,
    pub(crate) usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResponseMetrics {
    pub(crate) model: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) estimated_cost_microrubles: Option<u64>,
    pub(crate) premium: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) calls: Vec<CallUsage>,
}

impl ResponseMetrics {
    pub(crate) fn new(
        model: impl Into<String>,
        elapsed_ms: u64,
        usage: Option<TokenUsage>,
        prices: &PriceCatalog,
    ) -> Self {
        let model = model.into();
        let estimate = usage.and_then(|usage| prices.estimate(&model, usage));
        Self {
            model,
            elapsed_ms,
            usage,
            estimated_cost_microrubles: estimate.map(|estimate| estimate.microrubles),
            premium: estimate.map(|estimate| estimate.premium),
            calls: Vec::new(),
        }
    }

    pub(crate) fn refresh_cost(&mut self, prices: &PriceCatalog) {
        if !self.calls.is_empty() {
            let total = self
                .calls
                .iter()
                .try_fold((0_u64, false), |(cost, premium), call| {
                    let estimate = prices.estimate(&call.model, call.usage?)?;
                    Some((
                        cost.saturating_add(estimate.microrubles),
                        premium || estimate.premium,
                    ))
                });
            self.estimated_cost_microrubles = total.map(|(cost, _)| cost);
            self.premium = total.map(|(_, premium)| premium);
            return;
        }
        let estimate = self
            .usage
            .and_then(|usage| prices.estimate(&self.model, usage));
        self.estimated_cost_microrubles = estimate.map(|estimate| estimate.microrubles);
        self.premium = estimate.map(|estimate| estimate.premium);
    }

    pub(crate) fn from_calls(
        model: &str,
        elapsed_ms: u64,
        calls: Vec<CallUsage>,
        prices: &PriceCatalog,
    ) -> Self {
        let usage = calls
            .iter()
            .try_fold(TokenUsage::default(), |mut total, call| {
                let usage = call.usage?;
                total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
                total.completion_tokens = total
                    .completion_tokens
                    .saturating_add(usage.completion_tokens);
                total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
                total.cached_prompt_tokens = total
                    .cached_prompt_tokens
                    .saturating_add(usage.cached_prompt_tokens);
                Some(total)
            });
        let mut metrics = Self::new(model, elapsed_ms, usage, prices);
        metrics.calls = calls;
        metrics.refresh_cost(prices);
        metrics
    }
}

pub(crate) fn format_duration(elapsed_ms: u64) -> String {
    if elapsed_ms < 1_000 {
        format!("{elapsed_ms} мс")
    } else {
        format!("{:.2} с", elapsed_ms as f64 / 1_000.0).replace('.', ",")
    }
}

pub(crate) fn format_tokens(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(' ');
        }
        formatted.push(character);
    }
    formatted
}

pub(crate) fn format_cost(microrubles: u64) -> String {
    let rubles = microrubles / 1_000_000;
    let fraction = microrubles % 1_000_000;
    format!("{rubles},{fraction:06} ₽")
}

pub(crate) fn metric_lines(metrics: Option<&ResponseMetrics>) -> [String; 3] {
    let Some(metrics) = metrics else {
        return [
            "👉 Время ответа: —".to_owned(),
            "👉 Токены: —".to_owned(),
            "👉 Стоимость: —".to_owned(),
        ];
    };
    let tokens = metrics.usage.map_or_else(
        || "👉 Токены: нет данных API".to_owned(),
        |usage| {
            format!(
                "👉 Токены: {} · вход {} · выход {}",
                format_tokens(usage.total_tokens),
                format_tokens(usage.prompt_tokens),
                format_tokens(usage.completion_tokens)
            )
        },
    );
    let cost = metrics.estimated_cost_microrubles.map_or_else(
        || "👉 Стоимость: нет данных".to_owned(),
        |cost| {
            format!(
                "👉 Стоимость: ≈ {} · расчёт по токенному прайсу",
                format_cost(cost)
            )
        },
    );
    [
        format!("👉 Время ответа: {}", format_duration(metrics.elapsed_ms)),
        tokens,
        cost,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_cost_by_model_and_does_not_report_partial_totals() {
        let prices = PriceCatalog::with_prices(&[("main", 10.0, 20.0), ("child", 30.0, 40.0)]);
        let usage = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_prompt_tokens: 0,
        };
        let calls = vec![
            CallUsage {
                model: "main".into(),
                usage: Some(usage),
            },
            CallUsage {
                model: "child".into(),
                usage: Some(usage),
            },
        ];
        let metrics = ResponseMetrics::from_calls("main", 123, calls.clone(), &prices);
        assert_eq!(metrics.usage.expect("total").total_tokens, 30);
        assert_eq!(metrics.estimated_cost_microrubles, Some(700));
        assert_eq!(metrics.elapsed_ms, 123);
        let mut unavailable = ResponseMetrics::from_calls(
            "main",
            123,
            calls.clone(),
            &PriceCatalog::with_price("main", 10.0, 20.0, false),
        );
        assert_eq!(unavailable.estimated_cost_microrubles, None);
        unavailable.refresh_cost(&prices);
        assert_eq!(unavailable.estimated_cost_microrubles, Some(700));
        let mut calls = calls;
        calls[1].usage = None;
        let unknown = ResponseMetrics::from_calls("main", 123, calls, &prices);
        assert!(unknown.usage.is_none());
        assert!(unknown.estimated_cost_microrubles.is_none());
    }

    #[test]
    fn formats_metrics_for_russian_terminal() {
        assert_eq!(format_duration(842), "842 мс");
        assert_eq!(format_duration(2_345), "2,35 с");
        assert_eq!(format_tokens(1_234_567), "1 234 567");
        assert_eq!(format_cost(42_137), "0,042137 ₽");
    }

    #[test]
    fn displays_unavailable_metrics_without_guessing() {
        assert_eq!(
            metric_lines(None),
            ["👉 Время ответа: —", "👉 Токены: —", "👉 Стоимость: —"]
        );
    }
}
