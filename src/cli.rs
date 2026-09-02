use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum EditMode {
    /// Привычные сочетания клавиш Emacs (Ctrl+A, Ctrl+E и другие).
    #[default]
    Emacs,
    /// Режимы вставки и навигации Vim.
    Vim,
}

/// Интерактивный CLI-клиент AI-сервиса NeuralDeep.
#[derive(Debug, Parser)]
#[command(name = "agi", version, about)]
pub(crate) struct Cli {
    /// Необязательный первый вопрос после запуска.
    pub(crate) question: Option<String>,

    /// Режим редактирования строки ввода.
    #[arg(long, value_enum, default_value_t)]
    pub(crate) edit_mode: EditMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_question() {
        let cli = Cli::try_parse_from(["agi", "Объясни ownership в Rust"])
            .expect("question should be accepted");

        assert_eq!(cli.question.as_deref(), Some("Объясни ownership в Rust"));
        assert_eq!(cli.edit_mode, EditMode::Emacs);
    }

    #[test]
    fn starts_without_question() {
        let cli = Cli::try_parse_from(["agi"]).expect("interactive mode should be accepted");

        assert_eq!(cli.question, None);
        assert_eq!(cli.edit_mode, EditMode::Emacs);
    }

    #[test]
    fn enables_vim_editing_mode() {
        let cli = Cli::try_parse_from(["agi", "--edit-mode", "vim"])
            .expect("Vim mode should be accepted");

        assert_eq!(cli.edit_mode, EditMode::Vim);
    }

    #[test]
    fn rejects_extra_arguments() {
        assert!(Cli::try_parse_from(["agi", "первый", "второй"]).is_err());
    }
}
