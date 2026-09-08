use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::input::LineInput;

const DEFAULT_MAX_TOKENS: u32 = 10000;
const DEFAULT_TEMPERATURE: f32 = 0.1;
use crate::config::{DEFAULT_CONTEXT_TOKENS, DEFAULT_MODEL};
const MIN_STRUCTURED_TOKENS: u32 = 256;
const MIN_TEMPERATURE: f32 = 0.0;
const MAX_TEMPERATURE: f32 = 2.0;

const MODEL_OPTIONS: &[ModelOption] = &[
    ModelOption::included("gpt-oss-20b"),
    ModelOption::outside_subscription("qwen3.7-flash"),
    ModelOption::included("gpt-oss-120b"),
    ModelOption::included("qwen3.6-35b-a3b"),
    ModelOption::included("qwen3.6-35b-a3b-noreason"),
    ModelOption::included("qwen3.6-fp8"),
    ModelOption::included("qwen3.6-fp8-noreason"),
    ModelOption::outside_subscription("glm-5.3-flash"),
    ModelOption::included("gemma-4-31b"),
    ModelOption::included("gemma-4-31b-noreason"),
    ModelOption::included("qwen3.8-27b"),
    ModelOption::included("qwen3.8-27b-noreason"),
    ModelOption::outside_subscription("deepseek-v4-flash-vision-exp"),
    ModelOption::outside_subscription("deepseek-v4-flash"),
    ModelOption::outside_subscription("minimax-m2.5"),
    ModelOption::outside_subscription("qwen3.7-plus"),
    ModelOption::included("kimi-k2.6"),
    ModelOption::outside_subscription("kimi-k2.7-code"),
    ModelOption::outside_subscription("deepseek-v4-pro"),
    ModelOption::outside_subscription("glm-5.2"),
    ModelOption::outside_subscription("kimi-k3"),
];

#[derive(Clone, Copy)]
struct ModelOption {
    name: &'static str,
    outside_subscription: bool,
}

impl ModelOption {
    const fn included(name: &'static str) -> Self {
        Self {
            name,
            outside_subscription: false,
        }
    }

    const fn outside_subscription(name: &'static str) -> Self {
        Self {
            name,
            outside_subscription: true,
        }
    }

    fn label(self) -> String {
        if self.outside_subscription {
            format!("{} — вне подписки", self.name)
        } else {
            self.name.to_owned()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionCondition {
    None,
    StopSequence(String),
    Instruction(String),
}

impl fmt::Display for CompletionCondition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("не задано"),
            Self::StopSequence(sequence) => write!(formatter, "stop sequence: {sequence:?}"),
            Self::Instruction(instruction) => write!(formatter, "инструкция: {instruction}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    model: String,
    response_format_enabled: bool,
    max_tokens: u32,
    context_tokens: u32,
    temperature: f32,
    completion_condition: CompletionCondition,
    system_prompt: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.to_owned(),
            response_format_enabled: false,
            max_tokens: DEFAULT_MAX_TOKENS,
            context_tokens: DEFAULT_CONTEXT_TOKENS,
            temperature: DEFAULT_TEMPERATURE,
            completion_condition: CompletionCondition::None,
            system_prompt: None,
        }
    }
}

impl Settings {
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn response_format_enabled(&self) -> bool {
        self.response_format_enabled
    }

    pub(crate) fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    pub(crate) fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    pub(crate) fn set_context_tokens(&mut self, value: &str) -> Result<(), String> {
        let limit = value
            .parse::<u32>()
            .map_err(|_| "Контекст должен быть целым положительным числом".to_owned())?;
        if limit <= self.max_tokens {
            return Err("Контекст должен быть больше лимита ответа (max_tokens)".into());
        }
        self.context_tokens = limit;
        Ok(())
    }

    pub(crate) fn temperature(&self) -> f32 {
        self.temperature
    }

    pub(crate) fn stop_sequence(&self) -> Option<&str> {
        match &self.completion_condition {
            CompletionCondition::StopSequence(sequence) => Some(sequence),
            CompletionCondition::None | CompletionCondition::Instruction(_) => None,
        }
    }

    pub(crate) fn completion_instruction(&self) -> Option<&str> {
        match &self.completion_condition {
            CompletionCondition::Instruction(instruction) => Some(instruction),
            CompletionCondition::None | CompletionCondition::StopSequence(_) => None,
        }
    }

    pub(crate) fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    pub(crate) fn effective_system_prompt(&self) -> Option<String> {
        let mut prompt = self.system_prompt.clone();
        if let Some(instruction) = self.completion_instruction() {
            let instruction =
                format!("Обязательно соблюдай это условие завершения ответа: {instruction}");
            match prompt.as_mut() {
                Some(prompt) => {
                    prompt.push_str("\n\n");
                    prompt.push_str(&instruction);
                }
                None => prompt = Some(instruction),
            }
        }
        prompt
    }

    pub(crate) fn menu_items(&self) -> Vec<String> {
        vec![
            format!("Модель: {}", self.model),
            format!(
                "Structured Output: {}",
                if self.response_format_enabled {
                    "включен"
                } else {
                    "выключен"
                }
            ),
            format!("Максимальная длина: {} токенов", self.max_tokens),
            format!("Температура: {}", format_temperature(self.temperature)),
            format!("Завершение ответа: {}", self.completion_condition),
            format!(
                "Системный prompt: {}",
                if self.system_prompt.is_some() {
                    "задан"
                } else {
                    "не задан"
                }
            ),
            format!("Контекст: {} токенов", self.context_tokens),
        ]
    }

    pub(crate) fn model_items() -> Vec<String> {
        MODEL_OPTIONS
            .iter()
            .copied()
            .map(ModelOption::label)
            .collect()
    }

    pub(crate) fn model_index(&self) -> usize {
        MODEL_OPTIONS
            .iter()
            .position(|model| model.name == self.model)
            .unwrap_or(0)
    }

    pub(crate) fn select_model(&mut self, choice: usize) -> bool {
        let Some(model) = MODEL_OPTIONS.get(choice) else {
            return false;
        };
        if self.model == model.name {
            return false;
        }
        self.model = model.name.to_owned();
        true
    }

    pub(crate) fn set_response_format(&mut self, enabled: bool) -> Option<String> {
        self.response_format_enabled = enabled;
        if enabled && self.max_tokens < MIN_STRUCTURED_TOKENS {
            self.max_tokens = MIN_STRUCTURED_TOKENS;
            Some(format!(
                "Для Structured Output лимит увеличен до {MIN_STRUCTURED_TOKENS} токенов."
            ))
        } else {
            None
        }
    }

    pub(crate) fn set_max_tokens(&mut self, value: &str) -> Result<Option<String>, String> {
        let max_tokens = value
            .parse::<u32>()
            .map_err(|_| "Требуется целое число больше нуля.".to_owned())?;
        if max_tokens == 0 {
            return Err("Требуется целое число больше нуля.".to_owned());
        }
        if max_tokens >= self.context_tokens {
            return Err("Лимит ответа должен быть меньше окна контекста".into());
        }
        if self.response_format_enabled && max_tokens < MIN_STRUCTURED_TOKENS {
            return Err(format!(
                "Для Structured Output требуется минимум {MIN_STRUCTURED_TOKENS} токенов."
            ));
        }
        self.max_tokens = max_tokens;
        Ok((max_tokens < 128)
            .then(|| "При лимите меньше 128 токенов ответ будет очень кратким.".to_owned()))
    }

    pub(crate) fn set_temperature(&mut self, value: &str) -> Result<(), String> {
        let normalized = value.replace(',', ".");
        let temperature = normalized
            .parse::<f32>()
            .map_err(|_| "Температура должна быть числом от 0.0 до 2.0.".to_owned())?;
        if !temperature.is_finite() || !(MIN_TEMPERATURE..=MAX_TEMPERATURE).contains(&temperature) {
            return Err("Температура должна быть числом от 0.0 до 2.0.".to_owned());
        }
        self.temperature = if temperature == 0.0 { 0.0 } else { temperature };
        Ok(())
    }

    pub(crate) fn clear_completion_condition(&mut self) {
        self.completion_condition = CompletionCondition::None;
    }

    pub(crate) fn set_stop_sequence(&mut self, value: String) -> Result<(), String> {
        if value.is_empty() {
            return Err("Строка остановки не должна быть пустой.".to_owned());
        }
        if value.chars().all(|character| character.is_ascii_digit()) {
            return Err(
                "Число похоже на лимит длины. Используйте «Максимальная длина».".to_owned(),
            );
        }
        self.completion_condition = CompletionCondition::StopSequence(value);
        Ok(())
    }

    pub(crate) fn set_completion_instruction(&mut self, value: String) -> Result<(), String> {
        if value.is_empty() {
            return Err("Инструкция не должна быть пустой.".to_owned());
        }
        self.completion_condition = CompletionCondition::Instruction(value);
        Ok(())
    }

    pub(crate) fn set_system_prompt(&mut self, value: String) -> Result<(), String> {
        if value.is_empty() {
            return Err("Системный prompt не должен быть пустым.".to_owned());
        }
        self.system_prompt = Some(value);
        Ok(())
    }

    pub(crate) fn clear_system_prompt(&mut self) {
        self.system_prompt = None;
    }

    pub(crate) fn configure<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<bool> {
        let original = self.clone();

        loop {
            let items = self.menu_items();
            let Some(choice) = input.select("Настройки текущего чата — Esc: назад", &items)?
            else {
                return Ok(*self != original);
            };

            match choice {
                0 => self.configure_model(input)?,
                1 => self.configure_response_format(input, output)?,
                2 => self.configure_max_tokens(input, output)?,
                3 => self.configure_temperature(input, output)?,
                4 => self.configure_completion(input, output)?,
                5 => self.configure_system_prompt(input, output)?,
                6 => {
                    if let Some(value) =
                        input.read_line("Контекст в токенах (пустая строка — отмена): ")?
                        && !value.trim().is_empty()
                        && let Err(error) = self.set_context_tokens(value.trim())
                    {
                        writeln!(output, "{error}")?;
                    }
                }
                _ => {}
            }
        }
    }

    fn configure_model<I: LineInput>(&mut self, input: &mut I) -> io::Result<()> {
        let items = Self::model_items();
        let Some(choice) = input.select("Модель — Esc: назад", &items)? else {
            return Ok(());
        };

        self.select_model(choice);
        Ok(())
    }

    fn configure_response_format<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        let items = vec!["Включить".to_owned(), "Выключить".to_owned()];
        let Some(choice) = input.select("Structured Output — Esc: назад", &items)? else {
            return Ok(());
        };

        match choice {
            0 => {
                self.response_format_enabled = true;
                if self.max_tokens < MIN_STRUCTURED_TOKENS {
                    self.max_tokens = MIN_STRUCTURED_TOKENS;
                    writeln!(
                        output,
                        "Для Structured Output лимит автоматически увеличен до {MIN_STRUCTURED_TOKENS} токенов."
                    )?;
                }
            }
            1 => self.response_format_enabled = false,
            _ => {}
        }
        Ok(())
    }

    fn configure_temperature<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        let Some(value) = input
            .read_line("Температура (0.0–2.0; точка или запятая; пустая строка — отмена): ")?
        else {
            return Ok(());
        };
        if value.is_empty() {
            return Ok(());
        }

        let normalized = value.replace(',', ".");
        match normalized.parse::<f32>() {
            Ok(temperature)
                if temperature.is_finite()
                    && (MIN_TEMPERATURE..=MAX_TEMPERATURE).contains(&temperature) =>
            {
                self.temperature = if temperature == 0.0 { 0.0 } else { temperature };
                writeln!(
                    output,
                    "Температура установлена: {}.",
                    format_temperature(self.temperature)
                )?;
            }
            _ => writeln!(
                output,
                "Значение не изменено: температура должна быть числом от 0.0 до 2.0."
            )?,
        }
        Ok(())
    }

    fn configure_max_tokens<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        let Some(value) =
            input.read_line("Максимальное количество токенов (> 0; пустая строка — отмена): ")?
        else {
            return Ok(());
        };
        if value.is_empty() {
            return Ok(());
        }

        match value.parse::<u32>() {
            Ok(max_tokens)
                if self.response_format_enabled && max_tokens < MIN_STRUCTURED_TOKENS =>
            {
                writeln!(
                    output,
                    "Для Structured Output требуется минимум {MIN_STRUCTURED_TOKENS} токенов; значение не изменено."
                )?;
            }
            Ok(max_tokens) if max_tokens >= self.context_tokens => {
                writeln!(output, "Лимит ответа должен быть меньше окна контекста")?;
            }
            Ok(max_tokens) if max_tokens > 0 => {
                self.max_tokens = max_tokens;
                if max_tokens < 128 {
                    writeln!(
                        output,
                        "Предупреждение: при лимите меньше 128 токенов ответ будет очень кратким."
                    )?;
                }
            }
            _ => writeln!(
                output,
                "Значение не изменено: требуется целое число больше нуля."
            )?,
        }
        Ok(())
    }

    fn configure_completion<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        let items = vec![
            "Без условия".to_owned(),
            "Stop sequence".to_owned(),
            "Явная инструкция для модели".to_owned(),
        ];
        let Some(choice) = input.select("Условие завершения — Esc: назад", &items)?
        else {
            return Ok(());
        };

        match choice {
            0 => self.completion_condition = CompletionCondition::None,
            1 => {
                if let Some(sequence) = read_non_empty(
                    input,
                    output,
                    "Строка остановки (например, <END>; пустая строка — отмена): ",
                )? {
                    if sequence.chars().all(|character| character.is_ascii_digit()) {
                        writeln!(
                            output,
                            "Число похоже на лимит длины. Используйте пункт «Максимальная длина»."
                        )?;
                    } else {
                        writeln!(
                            output,
                            "Ответ остановится перед первым точным вхождением {sequence:?}."
                        )?;
                        self.completion_condition = CompletionCondition::StopSequence(sequence);
                    }
                }
            }
            2 => {
                if let Some(instruction) = read_non_empty(input, output, "Инструкция завершения: ")?
                {
                    self.completion_condition = CompletionCondition::Instruction(instruction);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn configure_system_prompt<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        let source = if self.system_prompt.is_none() && self.completion_instruction().is_some() {
            "только инструкция завершения"
        } else {
            "пользовательский"
        };
        match self.effective_system_prompt() {
            Some(current_prompt) => {
                let current_prompt = terminal_safe(&current_prompt);
                writeln!(
                    output,
                    "\nТекущий системный prompt ({source}):\n{current_prompt}\n"
                )?;
            }
            None => writeln!(output, "\nТекущий системный prompt: не задан.\n")?,
        }
        output.flush()?;

        let items = vec![
            "Задать или изменить".to_owned(),
            "Удалить системный prompt".to_owned(),
        ];
        let Some(choice) = input.select("Системный prompt — Esc: назад", &items)?
        else {
            return Ok(());
        };

        match choice {
            0 => {
                if let Some(prompt) =
                    read_non_empty(input, output, "Системный prompt (пустая строка — отмена): ")?
                {
                    self.system_prompt = Some(prompt);
                    writeln!(output, "Пользовательский системный prompt сохранён.")?;
                }
            }
            1 => {
                self.system_prompt = None;
                writeln!(output, "Системный prompt удалён.")?;
                if self.completion_instruction().is_some() {
                    writeln!(
                        output,
                        "Явная инструкция завершения ответа остаётся активной."
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if !character.is_control() || matches!(character, '\n' | '\t') {
                character
            } else {
                '�'
            }
        })
        .collect()
}

fn format_temperature(temperature: f32) -> String {
    if temperature.fract() == 0.0 {
        format!("{temperature:.1}")
    } else {
        temperature.to_string()
    }
}

fn read_non_empty<I: LineInput, W: Write>(
    input: &mut I,
    output: &mut W,
    prompt: &str,
) -> io::Result<Option<String>> {
    let Some(value) = input.read_line(prompt)? else {
        return Ok(None);
    };

    if value.is_empty() {
        writeln!(
            output,
            "Значение не изменено: строка не должна быть пустой."
        )?;
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::input::BufferedInput;

    #[test]
    fn uses_safe_defaults() {
        let settings = Settings::default();

        assert_eq!(settings.model(), DEFAULT_MODEL);
        assert!(!settings.response_format_enabled());
        assert_eq!(settings.max_tokens(), DEFAULT_MAX_TOKENS);
        assert_eq!(settings.temperature(), DEFAULT_TEMPERATURE);
        assert_eq!(settings.stop_sequence(), None);
        assert_eq!(settings.completion_instruction(), None);
        assert_eq!(settings.system_prompt(), None);
        assert_eq!(settings.effective_system_prompt(), None);
    }

    #[test]
    fn configures_all_chat_settings_with_menu_choices() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new(
            "1\n3\n2\n1\n3\n1200\n4\n0,7\n5\n2\n<END>\n6\n1\nТы редактор\nesc\n",
        ));
        let mut output = Vec::new();

        let changed = settings
            .configure(&mut input, &mut output)
            .expect("settings should be configured");

        assert!(changed);
        assert_eq!(settings.model(), "gpt-oss-120b");
        assert!(settings.response_format_enabled());
        assert_eq!(settings.max_tokens(), 1200);
        assert_eq!(settings.temperature(), 0.7);
        assert_eq!(settings.stop_sequence(), Some("<END>"));
        assert_eq!(settings.completion_instruction(), None);
        assert_eq!(settings.system_prompt(), Some("Ты редактор"));
    }

    #[test]
    fn builds_explicit_completion_instruction() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("5\n3\nЗаверши словом ГОТОВО\nesc\n"));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("settings should be configured");

        assert_eq!(settings.stop_sequence(), None);
        assert_eq!(
            settings.completion_instruction(),
            Some("Заверши словом ГОТОВО")
        );
        assert_eq!(
            settings.effective_system_prompt().as_deref(),
            Some("Обязательно соблюдай это условие завершения ответа: Заверши словом ГОТОВО")
        );
    }

    #[test]
    fn escape_keeps_values_unchanged() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("esc\n"));

        let changed = settings
            .configure(&mut input, &mut Vec::new())
            .expect("settings menu should close");

        assert!(!changed);
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn rejects_numeric_stop_sequence_as_likely_token_limit() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("5\n2\n400\nesc\n"));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("settings menu should recover");

        assert_eq!(settings.stop_sequence(), None);
        assert!(
            String::from_utf8(output)
                .expect("output should be UTF-8")
                .contains("похоже на лимит длины")
        );
    }

    #[test]
    fn enforces_minimum_token_limit_for_structured_output() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("3\n100\n2\n1\n3\n100\nesc\n"));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("settings should be configured");

        assert!(settings.response_format_enabled());
        assert_eq!(settings.max_tokens(), MIN_STRUCTURED_TOKENS);
        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains("автоматически увеличен"));
        assert!(output.contains("требуется минимум"));
    }

    #[test]
    fn clears_custom_system_prompt() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("6\n1\nОсобые правила\n6\n2\nesc\n"));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("system prompt should be cleared");

        assert_eq!(settings.system_prompt(), None);
        assert_eq!(settings.effective_system_prompt(), None);
        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains("Текущий системный prompt: не задан."));
        assert!(output.contains("Текущий системный prompt (пользовательский):\nОсобые правила"));
        assert!(output.contains("Системный prompt удалён."));
    }

    #[test]
    fn displays_the_exact_effective_system_prompt() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new(
            "6\n1\nМой prompt\n5\n3\nЗаверши словом Готово\n6\nesc\nesc\n",
        ));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("settings should be configured");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains(
            "Текущий системный prompt (пользовательский):\nМой prompt\n\nОбязательно соблюдай это условие завершения ответа: Заверши словом Готово"
        ));
    }

    #[test]
    fn accepts_temperature_boundaries_and_decimal_comma() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("0\n2.0\n0,75\n"));
        let mut output = Vec::new();

        settings
            .configure_temperature(&mut input, &mut output)
            .expect("zero temperature should be accepted");
        assert_eq!(settings.temperature(), 0.0);
        settings
            .configure_temperature(&mut input, &mut output)
            .expect("maximum temperature should be accepted");
        assert_eq!(settings.temperature(), 2.0);
        settings
            .configure_temperature(&mut input, &mut output)
            .expect("decimal comma should be accepted");
        assert_eq!(settings.temperature(), 0.75);

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains("Температура установлена: 0.0."));
        assert!(output.contains("Температура установлена: 2.0."));
        assert!(output.contains("Температура установлена: 0.75."));
    }

    #[test]
    fn defaults_new_and_legacy_settings_to_200000_context_tokens() {
        assert_eq!(Settings::default().context_tokens(), 200_000);
        let legacy: Settings =
            serde_json::from_str(r#"{"max_tokens":900}"#).expect("legacy settings");
        assert_eq!(legacy.context_tokens(), 200_000);
        assert_eq!(legacy.max_tokens(), 900);
    }

    #[test]
    fn context_budget_roundtrips_and_reserves_output() {
        let mut settings = Settings::default();
        for invalid in ["0", "-1", "abc", "10000"] {
            assert!(settings.set_context_tokens(invalid).is_err());
            assert_eq!(settings.context_tokens(), 200_000);
        }
        settings.set_context_tokens("120000").expect("context");
        assert!(settings.set_max_tokens("120000").is_err());
        let saved = serde_json::to_string(&settings).expect("serialize");
        let restored: Settings = serde_json::from_str(&saved).expect("restore");
        assert_eq!(restored.context_tokens(), 120_000);
        assert_eq!(restored.max_tokens(), 10_000);
    }

    #[test]
    fn rejects_invalid_temperature_without_changing_it() {
        for value in ["-0.1", "2.1", "NaN", "inf", "не число"] {
            let mut settings = Settings::default();
            let mut input = BufferedInput::new(Cursor::new(format!("{value}\n")));
            let mut output = Vec::new();

            settings
                .configure_temperature(&mut input, &mut output)
                .expect("invalid value should be handled");

            assert_eq!(settings.temperature(), DEFAULT_TEMPERATURE);
            assert!(
                String::from_utf8(output)
                    .expect("output should be UTF-8")
                    .contains("температура должна быть числом от 0.0 до 2.0")
            );
        }
    }

    #[test]
    fn old_settings_without_temperature_use_the_default() {
        let settings: Settings =
            serde_json::from_str(r#"{"max_tokens":900}"#).expect("old settings should deserialize");

        assert_eq!(settings.model(), DEFAULT_MODEL);
        assert_eq!(settings.max_tokens(), 900);
        assert_eq!(settings.temperature(), DEFAULT_TEMPERATURE);
    }

    #[test]
    fn lists_all_models_and_marks_models_outside_subscription() {
        assert_eq!(MODEL_OPTIONS.len(), 21);
        assert_eq!(MODEL_OPTIONS[0].label(), "gpt-oss-20b");
        assert_eq!(MODEL_OPTIONS[1].label(), "qwen3.7-flash — вне подписки");
        assert_eq!(MODEL_OPTIONS[20].label(), "kimi-k3 — вне подписки");
    }
}
