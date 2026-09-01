use std::fmt;
use std::io::{self, Write};

use crate::input::LineInput;

const DEFAULT_MAX_TOKENS: u32 = 500;
const MIN_STRUCTURED_TOKENS: u32 = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
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
            Self::Instruction(instruction) => {
                write!(formatter, "инструкция: {instruction}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    response_format_enabled: bool,
    max_tokens: u32,
    completion_condition: CompletionCondition,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            response_format_enabled: false,
            max_tokens: DEFAULT_MAX_TOKENS,
            completion_condition: CompletionCondition::None,
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

    pub(crate) fn system_instruction(&self) -> Option<String> {
        if let CompletionCondition::Instruction(instruction) = &self.completion_condition {
            Some(format!(
                "Обязательно соблюдай это условие завершения ответа: {instruction}"
            ))
        } else {
            None
        }
    }

    pub(crate) fn configure<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        loop {
            self.print_summary(output)?;

            let Some(choice) = input.read_line("settings> ")? else {
                return Ok(());
            };

            match choice.as_str() {
                "0" | "/back" | "/exit" | "" => return Ok(()),
                "1" => self.configure_response_format(input, output)?,
                "2" => self.configure_max_tokens(input, output)?,
                "3" => self.configure_completion(input, output)?,
                _ => writeln!(output, "Неизвестный пункт. Введите 0, 1, 2 или 3.")?,
            }
        }
    }

    fn print_summary<W: Write>(&self, output: &mut W) -> io::Result<()> {
        writeln!(output, "\nНастройки текущей сессии:")?;
        writeln!(
            output,
            "  1. Structured Output (JSON Schema): {}",
            if self.response_format_enabled {
                "включен"
            } else {
                "выключен"
            }
        )?;
        writeln!(
            output,
            "  2. Максимальная длина: {} токенов",
            self.max_tokens
        )?;
        writeln!(
            output,
            "  3. Завершение ответа: {}",
            self.completion_condition
        )?;
        writeln!(output, "  0. Вернуться к диалогу")
    }

    fn configure_response_format<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        let Some(value) = input.read_line("Включить strict JSON Schema? [да/нет]: ")?
        else {
            return Ok(());
        };

        match value.to_lowercase().as_str() {
            "да" | "д" | "yes" | "y" | "on" => {
                self.response_format_enabled = true;
                if self.max_tokens < MIN_STRUCTURED_TOKENS {
                    self.max_tokens = MIN_STRUCTURED_TOKENS;
                    writeln!(
                        output,
                        "Для Structured Output лимит автоматически увеличен до {MIN_STRUCTURED_TOKENS} токенов."
                    )?;
                }
            }
            "нет" | "н" | "no" | "n" | "off" => self.response_format_enabled = false,
            _ => writeln!(output, "Значение не изменено: введите «да» или «нет».")?,
        }

        Ok(())
    }

    fn configure_max_tokens<I: LineInput, W: Write>(
        &mut self,
        input: &mut I,
        output: &mut W,
    ) -> io::Result<()> {
        let Some(value) = input
            .read_line("Максимальное количество токенов (> 0; малый лимит сокращает ответ): ")?
        else {
            return Ok(());
        };

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
        writeln!(output, "Выберите условие завершения:")?;
        writeln!(output, "  0. Без условия")?;
        writeln!(
            output,
            "  1. Stop sequence — буквальная строка остановки, не лимит токенов"
        )?;
        writeln!(output, "  2. Явная инструкция для модели")?;

        let Some(choice) = input.read_line("completion> ")? else {
            return Ok(());
        };

        match choice.as_str() {
            "0" => self.completion_condition = CompletionCondition::None,
            "1" => {
                if let Some(sequence) = read_non_empty(
                    input,
                    output,
                    "Строка остановки (например, <END>; избегайте обычных слов): ",
                )? {
                    if sequence.chars().all(|character| character.is_ascii_digit()) {
                        writeln!(
                            output,
                            "Число похоже на лимит длины. Используйте пункт 2 главного меню настроек."
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
            "2" => {
                if let Some(instruction) = read_non_empty(input, output, "Инструкция завершения: ")?
                {
                    self.completion_condition = CompletionCondition::Instruction(instruction);
                }
            }
            _ => writeln!(output, "Значение не изменено: введите 0, 1 или 2.")?,
        }

        Ok(())
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

        assert!(!settings.response_format_enabled());
        assert_eq!(settings.max_tokens(), 500);
        assert_eq!(settings.stop_sequence(), None);
        assert_eq!(settings.system_instruction(), None);
    }

    #[test]
    fn configures_format_length_and_stop_sequence() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("1\nда\n2\n1200\n3\n1\n<END>\n0\n"));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("settings should be configured");

        assert!(settings.response_format_enabled());
        assert_eq!(settings.max_tokens(), 1200);
        assert_eq!(settings.stop_sequence(), Some("<END>"));
        assert_eq!(settings.system_instruction(), None);
    }

    #[test]
    fn builds_explicit_completion_instruction() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("3\n2\nЗаверши словом ГОТОВО\n0\n"));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("settings should be configured");

        assert_eq!(settings.stop_sequence(), None);
        assert!(
            settings
                .system_instruction()
                .expect("completion instruction should exist")
                .contains("Заверши словом ГОТОВО")
        );
    }

    #[test]
    fn keeps_values_after_invalid_input() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("2\n0\n3\n1\n\n0\n"));
        let mut output = Vec::new();

        settings
            .configure(&mut input, &mut output)
            .expect("settings menu should recover");

        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn rejects_numeric_stop_sequence_as_likely_token_limit() {
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("3\n1\n400\n0\n"));
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
        let mut input = BufferedInput::new(Cursor::new("2\n100\n1\nда\n2\n100\n0\n"));
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
}
