use clap::Parser;

/// Задайте вопрос AI-сервису NeuralDeep.
#[derive(Debug, Parser)]
#[command(name = "agi", version, about)]
pub(crate) struct Cli {
    /// Вопрос для AI-сервиса.
    pub(crate) question: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_question() {
        let cli = Cli::try_parse_from(["agi", "Объясни ownership в Rust"])
            .expect("question should be accepted");

        assert_eq!(cli.question, "Объясни ownership в Rust");
    }

    #[test]
    fn rejects_missing_question() {
        assert!(Cli::try_parse_from(["agi"]).is_err());
    }

    #[test]
    fn rejects_extra_arguments() {
        assert!(Cli::try_parse_from(["agi", "первый", "второй"]).is_err());
    }
}
