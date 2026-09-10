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

impl TokenUsage {
    pub(crate) fn saturating_add(self, other: Self) -> Self {
        Self {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            cached_prompt_tokens: self
                .cached_prompt_tokens
                .saturating_add(other.cached_prompt_tokens),
        }
    }

    pub(crate) fn saturating_sub(self, other: Self) -> Self {
        Self {
            prompt_tokens: self.prompt_tokens.saturating_sub(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_sub(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_sub(other.total_tokens),
            cached_prompt_tokens: self
                .cached_prompt_tokens
                .saturating_sub(other.cached_prompt_tokens),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CallUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<crate::context::ContextReport>,
    pub(crate) model: String,
    pub(crate) usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResponseMetrics {
    #[serde(default)]
    pub(crate) is_summary: bool,
    pub(crate) model: String,
    pub(crate) elapsed_ms: u64,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) estimated_cost_microrubles: Option<u64>,
    pub(crate) premium: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) calls: Vec<CallUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cumulative_usage: Option<TokenUsage>,
    #[serde(skip)]
    pub(crate) already_counted_usage: Option<TokenUsage>,
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
            is_summary: false,
            model,
            elapsed_ms,
            usage,
            estimated_cost_microrubles: estimate.map(|estimate| estimate.microrubles),
            premium: estimate.map(|estimate| estimate.premium),
            calls: Vec::new(),
            cumulative_usage: None,
            already_counted_usage: None,
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

pub(crate) fn metric_lines(metrics: Option<&ResponseMetrics>) -> [String; 5] {
    let Some(metrics) = metrics else {
        return [
            "Время ответа: —",
            "Контекстное окно: —",
            "Выход последнего вызова: —",
            "API за весь диалог: —",
            "Стоимость: —",
        ]
        .map(str::to_owned);
    };
    let usage = metrics
        .calls
        .last()
        .map_or(metrics.usage, |call| call.usage);
    let tokens = |value: Option<u64>| {
        value.map_or_else(
            || "нет данных API".into(),
            |value| format!("{} токенов", format_tokens(value)),
        )
    };
    let model = metrics
        .calls
        .last()
        .map_or(metrics.model.as_str(), |call| call.model.as_str());
    let context = usage.map_or_else(
        || "нет данных API".into(),
        |usage| match crate::config::model_context_tokens(model) {
            Some(limit) => {
                let percent = usage.total_tokens as f64 * 100.0 / f64::from(limit);
                format!(
                    "{} / {} токенов ({:.1}%)",
                    format_tokens(usage.total_tokens),
                    format_tokens(limit as u64),
                    percent
                )
                .replace('.', ",")
            }
            None => format!("{} / неизвестно токенов", format_tokens(usage.total_tokens)),
        },
    );
    let cost = metrics.estimated_cost_microrubles.map_or_else(
        || "нет данных".into(),
        |cost| format!("≈ {}", format_cost(cost)),
    );
    [
        format!(
            "{}: {}",
            if metrics.is_summary {
                "Время суммаризации"
            } else {
                "Время ответа"
            },
            format_duration(metrics.elapsed_ms)
        ),
        format!("Контекстное окно: {}", context),
        format!(
            "Выход последнего вызова: {}",
            tokens(usage.map(|usage| usage.completion_tokens))
        ),
        format!(
            "API за весь диалог: {} · вход {} · выход {}",
            tokens(metrics.cumulative_usage.map(|usage| usage.total_tokens)),
            tokens(metrics.cumulative_usage.map(|usage| usage.prompt_tokens)),
            tokens(
                metrics
                    .cumulative_usage
                    .map(|usage| usage.completion_tokens)
            ),
        ),
        format!("Стоимость: {cost}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_labels_distinguish_api_input_from_replacement_history() {
        let mut metrics = ResponseMetrics::from_calls(
            "main",
            11130,
            vec![CallUsage {
                model: "main".into(),
                usage: Some(TokenUsage {
                    prompt_tokens: 29003,
                    completion_tokens: 2223,
                    total_tokens: 31226,
                    cached_prompt_tokens: 0,
                }),
                context: Some(crate::context::ContextReport {
                    limit: 200000,
                    before: 40000,
                    after: 40000,
                    output_reserve: 8192,
                    removed_messages: 0,
                }),
            }],
            &PriceCatalog::default(),
        );
        metrics.is_summary = true;
        metrics.cumulative_usage = metrics.usage;
        let lines = metric_lines(Some(&metrics));
        assert_eq!(lines[0], "Время суммаризации: 11,13 с");
        assert_eq!(lines[1], "Контекстное окно: 31 226 / неизвестно токенов");
        assert_eq!(lines[2], "Выход последнего вызова: 2 223 токенов");
        assert_eq!(
            lines[3],
            "API за весь диалог: 31 226 токенов · вход 29 003 токенов · выход 2 223 токенов"
        );
        metrics.calls[0].usage = None;
        assert!(metric_lines(Some(&metrics))[1].contains("нет данных API"));
    }

    #[test]
    fn displays_actual_final_prompt_without_reserves_or_other_calls() {
        let report = crate::context::ContextReport {
            limit: 200_000,
            before: 12_179,
            after: 12_179,
            output_reserve: 10_000,
            removed_messages: 0,
        };
        let mut metrics = ResponseMetrics::from_calls(
            "main",
            0,
            vec![
                CallUsage {
                    model: "child".into(),
                    context: None,
                    usage: Some(TokenUsage {
                        prompt_tokens: 5000,
                        ..TokenUsage::default()
                    }),
                },
                CallUsage {
                    model: "main".into(),
                    context: Some(report),
                    usage: Some(TokenUsage {
                        prompt_tokens: 100,
                        completion_tokens: 200,
                        total_tokens: 300,
                        cached_prompt_tokens: 50,
                    }),
                },
            ],
            &PriceCatalog::default(),
        );
        assert_eq!(
            metric_lines(Some(&metrics))[1],
            "Контекстное окно: 300 / неизвестно токенов"
        );
        metrics.calls[1].model = "deepseek-v4-flash".into();
        assert_eq!(
            metric_lines(Some(&metrics))[1],
            "Контекстное окно: 300 / 1 000 000 токенов (0,0%)"
        );
        metrics.calls[1].usage = None;
        assert_eq!(
            metric_lines(Some(&metrics))[1],
            "Контекстное окно: нет данных API"
        );
    }

    #[test]
    fn shows_context_decision_even_without_api_usage() {
        let metrics = ResponseMetrics::from_calls(
            "main",
            0,
            vec![CallUsage {
                model: "main".into(),
                usage: None,
                context: Some(crate::context::ContextReport {
                    limit: 2500,
                    before: 3112,
                    after: 2067,
                    output_reserve: 1000,
                    removed_messages: 2,
                }),
            }],
            &PriceCatalog::default(),
        );
        let lines = metric_lines(Some(&metrics));
        assert_eq!(lines[1], "Контекстное окно: нет данных API");
        assert!(!lines.join("\n").contains("2 067"));
        let restored: ResponseMetrics =
            serde_json::from_str(&serde_json::to_string(&metrics).expect("serialize"))
                .expect("deserialize");
        assert_eq!(restored, metrics);
    }

    #[test]
    fn loads_older_metrics() {
        let metrics: ResponseMetrics = serde_json::from_str(
            r#"{
            "model":"main", "elapsed_ms":0, "usage":null,
            "estimated_cost_microrubles":null, "premium":null
        }"#,
        )
        .expect("legacy metrics");
        assert_eq!(
            metric_lines(Some(&metrics))[1],
            "Контекстное окно: нет данных API"
        );
        assert_eq!(
            metric_lines(Some(&metrics))[2],
            "Выход последнего вызова: нет данных API"
        );
    }

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
                context: None,
                model: "main".into(),
                usage: Some(usage),
            },
            CallUsage {
                context: None,
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
            [
                "Время ответа: —",
                "Контекстное окно: —",
                "Выход последнего вызова: —",
                "API за весь диалог: —",
                "Стоимость: —"
            ]
        );
    }
}
