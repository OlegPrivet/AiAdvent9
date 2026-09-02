use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::input::LineInput;

const DEFAULT_MAX_TOKENS: u32 = 10000;
const MIN_STRUCTURED_TOKENS: u32 = 256;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Settings {
    response_format_enabled: bool,
    max_tokens: u32,
    completion_condition: CompletionCondition,
    system_prompt: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            response_format_enabled: false,
            max_tokens: DEFAULT_MAX_TOKENS,
            completion_condition: CompletionCondition::None,
            system_prompt: None,
        }
    }
}

impl Settings {
    pub(crate) fn response_format_enabled(&self) -> bool {
        self.response_format_enabled
    }

    pub(crate) fn max_tokens(&self) -> u32 {
        self.max_tokens
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

    #[cfg(test)]
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

    pub(crate) fn configure<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<bool> {
        let original = self.clone();

        loop {
            let items = vec![
                format!(
                    "Structured Output: {}",
                    if self.response_format_enabled {
                        "включен"
                    } else {
                        "выключен"
                    }
                ),
                format!("Максимальная длина: {} токенов", self.max_tokens),
                format!("Завершение ответа: {}", self.completion_condition),
                format!(
                    "Системный prompt: {}",
                    if self.system_prompt.is_some() {
                        "задан"
                    } else {
                        "не задан"
                    }
                ),
            ];
            let Some(choice) = input.select("Настройки текущего чата — Esc: назад", &items)?
            else {
                return Ok(*self != original);
            };

            match choice {
                0 => self.configure_response_format(input, output)?,
                1 => self.configure_max_tokens(input, output)?,
                2 => self.configure_completion(input, output)?,
                3 => self.configure_system_prompt(input, output)?,
                _ => {}
            }
        }
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

        assert!(!settings.response_format_enabled());
        assert_eq!(settings.max_tokens(), DEFAULT_MAX_TOKENS);
        assert_eq!(settings.stop_sequence(), None);
        assert_eq!(settings.completion_instruction(), None);
        assert_eq!(settings.system_prompt(), None);
        assert_eq!(settings.effective_system_prompt(), None);
    }

    #[test]
    fn configures_all_chat_settings_with_menu_choices() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new(
            "1\n1\n2\n1200\n3\n2\n<END>\n4\n1\nТы редактор\nesc\n",
        ));
        let mut output = Vec::new();

        let changed = settings
            .configure(&mut input, &mut output)
            .expect("settings should be configured");

        assert!(changed);
        assert!(settings.response_format_enabled());
        assert_eq!(settings.max_tokens(), 1200);
        assert_eq!(settings.stop_sequence(), Some("<END>"));
        assert_eq!(settings.completion_instruction(), None);
        assert_eq!(settings.system_prompt(), Some("Ты редактор"));
    }

    #[test]
    fn builds_explicit_completion_instruction() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("3\n3\nЗаверши словом ГОТОВО\nesc\n"));
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
        let mut input = BufferedInput::new(Cursor::new("3\n2\n400\nesc\n"));
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
        let mut input = BufferedInput::new(Cursor::new("2\n100\n1\n1\n2\n100\nesc\n"));
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
        let mut input = BufferedInput::new(Cursor::new("4\n1\nОсобые правила\n4\n2\nesc\n"));
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
            "4\n1\nМой prompt\n3\n3\nЗаверши словом Готово\n4\nesc\nesc\n",
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
}
