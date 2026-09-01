use clap::Parser;

/// Интерактивный CLI-клиент AI-сервиса NeuralDeep.
#[derive(Debug, Parser)]
#[command(name = "agi", version, about)]
pub(crate) struct Cli {
    /// Необязательный первый вопрос после запуска.
    pub(crate) question: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_question() {
        let cli = Cli::try_parse_from(["agi", "Объясни ownership в Rust"])
            .expect("question should be accepted");

        assert_eq!(cli.question.as_deref(), Some("Объясни ownership в Rust"));
    }

    #[test]
    fn starts_without_question() {
        let cli = Cli::try_parse_from(["agi"]).expect("interactive mode should be accepted");

        assert_eq!(cli.question, None);
    }

    #[test]
    fn rejects_extra_arguments() {
        assert!(Cli::try_parse_from(["agi", "первый", "второй"]).is_err());
    }
}
