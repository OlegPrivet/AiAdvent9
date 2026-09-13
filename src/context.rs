use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_WINDOW_MESSAGES: usize = 20;
pub(crate) const MIN_WINDOW_MESSAGES: usize = 2;
pub(crate) const MAX_WINDOW_MESSAGES: usize = 200;
pub(crate) const MAX_FACTS: usize = 50;
pub(crate) const MAX_FACT_KEY_CHARS: usize = 64;
pub(crate) const MAX_FACT_VALUE_CHARS: usize = 1000;

pub(crate) type Facts = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextStrategyKind {
    SlidingWindow,
    StickyFacts,
    Branching,
}

impl fmt::Display for ContextStrategyKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SlidingWindow => "Sliding Window",
            Self::StickyFacts => "Sticky Facts",
            Self::Branching => "Branching",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextStrategy {
    pub(crate) kind: ContextStrategyKind,
    pub(crate) max_messages: usize,
}

impl ContextStrategy {
    pub(crate) const fn sliding_default() -> Self {
        Self {
            kind: ContextStrategyKind::SlidingWindow,
            max_messages: DEFAULT_WINDOW_MESSAGES,
        }
    }

    pub(crate) const fn legacy_branching() -> Self {
        Self {
            kind: ContextStrategyKind::Branching,
            max_messages: DEFAULT_WINDOW_MESSAGES,
        }
    }

    pub(crate) fn validate_window(value: usize) -> Result<(), String> {
        if !(MIN_WINDOW_MESSAGES..=MAX_WINDOW_MESSAGES).contains(&value) || !value.is_multiple_of(2)
        {
            return Err(format!(
                "Размер окна должен быть чётным числом от {MIN_WINDOW_MESSAGES} до {MAX_WINDOW_MESSAGES}."
            ));
        }
        Ok(())
    }
}

impl Default for ContextStrategy {
    fn default() -> Self {
        Self::sliding_default()
    }
}

pub(crate) fn validate_fact(key: &str, value: &str) -> Result<(), String> {
    if key.is_empty()
        || key.chars().count() > MAX_FACT_KEY_CHARS
        || key
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(format!(
            "Ключ факта должен содержать 1–{MAX_FACT_KEY_CHARS} символа без пробелов."
        ));
    }
    if value.is_empty() || value.chars().count() > MAX_FACT_VALUE_CHARS {
        return Err(format!(
            "Значение факта должно содержать 1–{MAX_FACT_VALUE_CHARS} символов."
        ));
    }
    Ok(())
}

pub(crate) fn validate_facts(facts: &Facts) -> Result<(), String> {
    if facts.len() > MAX_FACTS {
        return Err(format!("Допускается не более {MAX_FACTS} фактов."));
    }
    for (key, value) in facts {
        validate_fact(key, value)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextReport {
    pub(crate) limit: u32,
    pub(crate) before: usize,
    pub(crate) after: usize,
    pub(crate) output_reserve: u32,
    pub(crate) removed_messages: usize,
}
