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
pub(crate) struct ResponseMetrics {
    pub(crate) model: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) estimated_cost_microrubles: Option<u64>,
    pub(crate) premium: Option<bool>,
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
        }
    }

    pub(crate) fn refresh_cost(&mut self, prices: &PriceCatalog) {
        let estimate = self
            .usage
            .and_then(|usage| prices.estimate(&self.model, usage));
        self.estimated_cost_microrubles = estimate.map(|estimate| estimate.microrubles);
        self.premium = estimate.map(|estimate| estimate.premium);
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
