use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use uuid::Uuid;

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

    /// Восстановить сохранённый чат по полному UUID.
    #[arg(long, value_name = "ID")]
    pub(crate) restore: Option<Uuid>,

    /// Внутренний stdio MCP-сервер для демонстрации.
    #[arg(long, hide = true)]
    pub(crate) mcp_demo_server: bool,

    #[command(subcommand)]
    pub(crate) rag: Option<RootCommand>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RootCommand {
    /// Локальная библиотека документов для ответов с источниками.
    Rag {
        #[command(subcommand)]
        command: RagCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RagCommand {
    /// Настройка сервиса эмбеддингов.
    Embeddings {
        #[command(subcommand)]
        command: EmbeddingsCommand,
    },
    /// Заново векторизовать все добавленные документы текущей моделью.
    Reindex,
    Add {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },
    List,
    Status,
    Remove {
        id: String,
    },
    Refresh,
    Search {
        query: String,
        #[arg(long, value_enum, default_value_t = crate::rag_pipeline::RelevanceMode::Off)]
        filter: crate::rag_pipeline::RelevanceMode,
        #[arg(long)]
        rewrite: bool,
        #[arg(long, default_value_t = crate::config::RAG_CANDIDATE_K)]
        candidate_k: usize,
        #[arg(long, default_value_t = crate::config::RAG_SIMILARITY_THRESHOLD)]
        similarity_threshold: f32,
        #[arg(long, default_value_t = crate::config::RAG_RERANK_THRESHOLD)]
        rerank_threshold: f32,
        #[arg(long, value_enum, default_value_t = RagStrategy::Structure)]
        strategy: RagStrategy,
        #[arg(long, default_value_t = 3)]
        limit: usize,
    },
    Compare {
        #[arg(long)]
        queries: Option<PathBuf>,
        #[arg(long)]
        eval: Option<PathBuf>,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Сравнить ответы модели с RAG и без RAG на контрольных вопросах.
    Evaluate {
        #[arg(long, value_parser = ["day22", "day23"], default_value = "day22")]
        suite: String,
        #[arg(long)]
        eval: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum EmbeddingsCommand {
    /// Показать модель, endpoint и состояние индекса.
    Show,
    /// Подключить OpenAI-совместимый endpoint эмбеддингов.
    Set {
        /// Полный URL endpoint, например http://localhost:8000/v1/embeddings.
        #[arg(long)]
        url: String,
        #[arg(long)]
        model: String,
        /// Имя переменной окружения с API-ключом; для локальной модели можно опустить.
        #[arg(long)]
        api_key_env: Option<String>,
    },
    /// Вернуться к NeuralDeep bge-m3.
    Reset,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum RagStrategy {
    Fixed,
    Structure,
}

impl From<RagStrategy> for crate::rag_chunk::Strategy {
    fn from(value: RagStrategy) -> Self {
        match value {
            RagStrategy::Fixed => Self::Fixed,
            RagStrategy::Structure => Self::Structure,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_day23_search_and_evaluation_options() {
        let cli = Cli::try_parse_from([
            "agi",
            "rag",
            "search",
            "Вопрос",
            "--filter",
            "rerank",
            "--rewrite",
            "--candidate-k",
            "20",
            "--limit",
            "5",
            "--similarity-threshold",
            "0.3",
            "--rerank-threshold",
            "0.6",
        ])
        .expect("search");
        assert!(matches!(
            cli.rag,
            Some(RootCommand::Rag {
                command: RagCommand::Search {
                    filter: crate::rag_pipeline::RelevanceMode::Rerank,
                    rewrite: true,
                    candidate_k: 20,
                    limit: 5,
                    ..
                }
            })
        ));
        let cli = Cli::try_parse_from([
            "agi",
            "rag",
            "evaluate",
            "--suite",
            "day23",
            "--eval",
            "cases.json",
            "--report",
            "report.md",
        ])
        .expect("evaluate");
        assert!(
            matches!(cli.rag,Some(RootCommand::Rag {command:RagCommand::Evaluate {suite,..}}) if suite == "day23")
        );
        assert!(
            Cli::try_parse_from([
                "agi",
                "rag",
                "evaluate",
                "--suite",
                "unknown",
                "--eval",
                "cases.json",
                "--report",
                "report.md"
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_question() {
        let cli = Cli::try_parse_from(["agi", "Объясни ownership в Rust"])
            .expect("question should be accepted");

        assert_eq!(cli.question.as_deref(), Some("Объясни ownership в Rust"));
        assert_eq!(cli.edit_mode, EditMode::Emacs);
        assert_eq!(cli.restore, None);
        assert!(!cli.mcp_demo_server);
    }

    #[test]
    fn starts_without_question() {
        let cli = Cli::try_parse_from(["agi"]).expect("interactive mode should be accepted");

        assert_eq!(cli.question, None);
        assert_eq!(cli.edit_mode, EditMode::Emacs);
        assert_eq!(cli.restore, None);
        assert!(!cli.mcp_demo_server);
    }

    #[test]
    fn enables_vim_editing_mode() {
        let cli = Cli::try_parse_from(["agi", "--edit-mode", "vim"])
            .expect("Vim mode should be accepted");

        assert_eq!(cli.edit_mode, EditMode::Vim);
    }

    #[test]
    fn parses_restore_id_with_initial_question() {
        let id = Uuid::new_v4();
        let cli = Cli::try_parse_from(["agi", "--restore", &id.to_string(), "Продолжи обсуждение"])
            .expect("restore arguments should be accepted");

        assert_eq!(cli.restore, Some(id));
        assert_eq!(cli.question.as_deref(), Some("Продолжи обсуждение"));
    }

    #[test]
    fn rejects_extra_arguments() {
        assert!(Cli::try_parse_from(["agi", "первый", "второй"]).is_err());
    }

    #[test]
    fn parses_user_selected_rag_paths() {
        let cli = Cli::try_parse_from(["agi", "rag", "add", "/tmp/manual.pdf", "/tmp/source code"])
            .expect("RAG paths should parse");
        assert!(matches!(
            cli.rag,
            Some(RootCommand::Rag {
                command: RagCommand::Add { paths, dry_run: false }
            }) if paths.len() == 2
        ));
    }

    #[test]
    fn parses_custom_embedding_endpoint_without_secret_value() {
        let cli = Cli::try_parse_from([
            "agi",
            "rag",
            "embeddings",
            "set",
            "--url",
            "http://localhost:8000/v1/embeddings",
            "--model",
            "my-embedding-model",
            "--api-key-env",
            "MY_EMBEDDING_KEY",
        ])
        .expect("embedding configuration should parse");
        assert!(matches!(
            cli.rag,
            Some(RootCommand::Rag {
                command: RagCommand::Embeddings {
                    command: EmbeddingsCommand::Set { model, api_key_env, .. }
                }
            }) if model == "my-embedding-model" && api_key_env.as_deref() == Some("MY_EMBEDDING_KEY")
        ));
    }
}
