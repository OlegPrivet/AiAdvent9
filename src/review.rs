//! Shared review configuration, history and non-interactive entry point.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::llm::LlmProfile;
use crate::metrics::TokenUsage;
use crate::rag::EmbeddingConfig;

#[derive(Debug, Error)]
pub(crate) enum ReviewError {
    #[error("{0}")]
    Invalid(String),
    #[error("ошибка файла ревью: {0}")]
    Io(#[from] std::io::Error),
    #[error("ошибка истории ревью: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("некорректные данные ревью: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ошибка GitHub: {0}")]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Rag(#[from] crate::rag::RagError),
    #[error(transparent)]
    Api(#[from] crate::api::ApiError),
}
pub(crate) type Result<T> = std::result::Result<T, ReviewError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Project {
    pub(crate) id: Uuid,
    pub(crate) path: PathBuf,
    pub(crate) repository: String,
    pub(crate) token_env: String,
    /// Relative tracked source paths; "." selects all safe text files.
    pub(crate) sources: Vec<String>,
    pub(crate) embedding: EmbeddingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReviewRun {
    pub(crate) id: Uuid,
    pub(crate) project_id: Uuid,
    pub(crate) repository: String,
    pub(crate) pr: Option<u64>,
    pub(crate) head: String,
    pub(crate) started: u64,
    pub(crate) status: String,
    pub(crate) publication: String,
    pub(crate) model: LlmProfile,
    pub(crate) elapsed_ms: u64,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) sources: Vec<String>,
    pub(crate) skipped: Vec<String>,
    pub(crate) markdown: String,
    pub(crate) error: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ReviewStore {
    pub(crate) directory: PathBuf,
}
impl ReviewStore {
    pub(crate) fn open() -> Result<Self> {
        Self::at(
            crate::chat::state_directory()
                .ok_or_else(|| ReviewError::Invalid("Не определён каталог состояния agi".into()))?,
        )
    }
    pub(crate) fn at(directory: PathBuf) -> Result<Self> {
        fs::create_dir_all(&directory)?;
        private(&directory, 0o700)?;
        let store = Self { directory };
        store.connection()?;
        private(&store.directory.join("review.sqlite3"), 0o600)?;
        Ok(store)
    }
    pub(crate) fn connection(&self) -> Result<Connection> {
        let connection = Connection::open(self.directory.join("review.sqlite3"))?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS review_projects(id TEXT PRIMARY KEY, project_json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS review_runs(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, started INTEGER NOT NULL, run_json TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS review_runs_project ON review_runs(project_id, started);")?;
        Ok(connection)
    }
    pub(crate) fn projects(&self) -> Result<Vec<Project>> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT project_json FROM review_projects ORDER BY rowid")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
    pub(crate) fn project(&self, id: Uuid) -> Result<Project> {
        let data: String = self.connection()?.query_row(
            "SELECT project_json FROM review_projects WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )?;
        Ok(serde_json::from_str(&data)?)
    }
    pub(crate) fn save_project(&self, project: &Project) -> Result<()> {
        validate_repository(&project.repository)?;
        validate_env(&project.token_env)?;
        validate_sources(&project.sources)?;
        self.connection()?.execute("INSERT INTO review_projects VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET project_json=excluded.project_json", params![project.id.to_string(), serde_json::to_string(project)?])?;
        Ok(())
    }
    pub(crate) fn save_run(&self, run: &ReviewRun) -> Result<()> {
        self.connection()?.execute("INSERT INTO review_runs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET run_json=excluded.run_json", params![run.id.to_string(), run.project_id.to_string(), run.started.min(i64::MAX as u64) as i64, serde_json::to_string(run)?])?;
        Ok(())
    }
    pub(crate) fn history(&self, project: Option<Uuid>) -> Result<Vec<ReviewRun>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT run_json FROM review_runs WHERE (?1 IS NULL OR project_id=?1) ORDER BY started DESC, rowid DESC LIMIT 200")?;
        let rows = statement.query_map([project.map(|id| id.to_string())], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}
pub(crate) fn private(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(crate) fn validate_repository(repository: &str) -> Result<()> {
    let parts: Vec<_> = repository.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || matches!(*part, "." | "..")
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        })
    {
        return Err(ReviewError::Invalid(
            "Репозиторий: OWNER/REPO на GitHub.com".into(),
        ));
    }
    Ok(())
}
pub(crate) fn validate_env(name: &str) -> Result<()> {
    if name.is_empty()
        || name.as_bytes()[0].is_ascii_digit()
        || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err(ReviewError::Invalid(
            "Нужно имя переменной окружения, не значение токена".into(),
        ));
    }
    Ok(())
}
pub(crate) fn validate_sources(sources: &[String]) -> Result<()> {
    if sources.is_empty()
        || sources.iter().any(|s| {
            s.is_empty()
                || Path::new(s).is_absolute()
                || Path::new(s).components().any(|c| {
                    !matches!(
                        c,
                        std::path::Component::Normal(_) | std::path::Component::CurDir
                    )
                })
        })
    {
        return Err(ReviewError::Invalid(
            "Источники: относительные пути внутри репозитория".into(),
        ));
    }
    Ok(())
}
pub(crate) fn remote_repository(remote: &str) -> Result<String> {
    let name = remote
        .trim()
        .strip_prefix("git@github.com:")
        .or_else(|| remote.trim().strip_prefix("https://github.com/"))
        .or_else(|| remote.trim().strip_prefix("ssh://git@github.com/"))
        .ok_or_else(|| ReviewError::Invalid("origin должен указывать на GitHub.com".into()))?;
    let name = name.trim_end_matches('/').trim_end_matches(".git");
    validate_repository(name)?;
    Ok(name.into())
}

#[derive(Debug, Subcommand)]
pub(crate) enum ReviewCommand {
    /// Запустить ревью без UI. Настройка проектов: /review внутри agi.
    Run(ReviewArgs),
    /// Отслеживать PR в текущем процессе (для фонового режима используйте automation start).
    Watch {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Выполнить один цикл проверки и выйти.
        #[arg(long)]
        once: bool,
    },
    /// Управление отдельным фоновым процессом.
    Automation {
        #[command(subcommand)]
        command: crate::review_watch::ControlCommand,
    },
}
#[derive(Debug, Args)]
pub(crate) struct ReviewArgs {
    #[arg(long)]
    pub(crate) project: Uuid,
    #[arg(long, conflicts_with = "local", required_unless_present = "local")]
    pub(crate) pr: Option<u64>,
    #[arg(long, conflicts_with = "pr")]
    pub(crate) local: bool,
    #[arg(long, requires = "pr", conflicts_with = "local")]
    pub(crate) head: Option<String>,
    #[arg(long, requires = "pr", conflicts_with = "local")]
    pub(crate) publish: bool,
    #[arg(long)]
    pub(crate) output: PathBuf,
}

pub(crate) async fn cli(command: ReviewCommand) -> Result<()> {
    let args = match command {
        ReviewCommand::Run(args) => args,
        ReviewCommand::Watch { state_dir, once } => {
            let store = match state_dir {
                Some(path) => ReviewStore::at(path)?,
                None => ReviewStore::open()?,
            };
            return crate::review_watch::watch(&store, once).await;
        }
        ReviewCommand::Automation { command } => {
            return crate::review_watch::control(&ReviewStore::open()?, command).await;
        }
    };
    let store = ReviewStore::open()?;
    let project = store.project(args.project)?;
    let chats = crate::chat::ChatStore::open().map_err(|e| ReviewError::Invalid(e.to_string()))?;
    let model = chats
        .llms()
        .active()
        .map_err(|e| ReviewError::Invalid(e.to_string()))?;
    let run = tokio::select! {
        result = crate::review_pipeline::run(&store, project.clone(), model, args.pr, args.head.as_deref(), |stage| eprintln!("Ревью: {stage}")) => result?,
        _ = tokio::signal::ctrl_c() => return Err(ReviewError::Invalid("Ревью отменено".into())),
    };
    let mut run = run;
    if args.publish && run.status == "completed" {
        crate::review_github::publish(&store, &project, &mut run).await?;
    }
    fs::create_dir_all(&args.output)?;
    fs::write(
        args.output.join("review.md"),
        if run.markdown.is_empty() {
            run.error.as_deref().unwrap_or("Ревью не завершено")
        } else {
            &run.markdown
        },
    )?;
    fs::write(
        args.output.join("review.json"),
        serde_json::to_vec_pretty(&run)?,
    )?;
    if run.status != "completed" || run.publication == "failed" {
        return Err(ReviewError::Invalid(
            run.error.unwrap_or_else(|| "Ревью не завершено".into()),
        ));
    }
    println!("Отчёт: {}", args.output.join("review.md").display());
    Ok(())
}

pub(crate) fn workflow(project: &Project, binary: &Path) -> Result<String> {
    let binary = binary
        .to_str()
        .filter(|v| !v.contains(['\n', '\r']))
        .ok_or_else(|| ReviewError::Invalid("Некорректный путь agi".into()))?;
    // YAML single quotes escape a literal quote by doubling it.
    let binary = binary.replace('\'', "''");
    let id = project.id;
    Ok(format!(
        r#"name: AI review
on:
  pull_request:
    types: [opened, reopened, synchronize]
  workflow_dispatch:
    inputs:
      pr:
        description: 'Номер PR'
        required: true
        type: string
permissions:
  contents: read
  pull-requests: write
concurrency:
  group: agi-review-${{{{ github.repository }}}}-${{{{ github.event.pull_request.number || inputs.pr }}}}
  cancel-in-progress: true
jobs:
  review:
    if: github.event_name == 'workflow_dispatch' || github.event.pull_request.head.repo.full_name == github.repository
    runs-on: [self-hosted, macOS, ARM64]
    timeout-minutes: 20
    env:
      AGI_BINARY: '{binary}'
      GITHUB_TOKEN: ${{{{ github.token }}}}
      NEURALDEEP_API_KEY: ${{{{ secrets.NEURALDEEP_API_KEY }}}}
      REVIEW_PR: ${{{{ github.event.pull_request.number || inputs.pr }}}}
      REVIEW_HEAD: ${{{{ github.event.pull_request.head.sha }}}}
    steps:
      - name: Review PR with installed agi
        shell: bash
        run: |
          args=(review run --project {id} --pr "$REVIEW_PR" --publish --output "$RUNNER_TEMP/agi-review-${{{{ github.run_id }}}}")
          if [[ -n "$REVIEW_HEAD" ]]; then args+=(--head "$REVIEW_HEAD"); fi
          "$AGI_BINARY" "${{args[@]}}"
      - name: Upload review reports
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: agi-review-${{{{ github.run_id }}}}
          path: ${{{{ runner.temp }}}}/agi-review-${{{{ github.run_id }}}}/review.*
          if-no-files-found: warn
"#
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn test_project(directory: &Path, endpoint: &str) -> Project {
        let rag = crate::rag::RagService::for_test(
            directory.join("template.sqlite3"),
            None,
            crate::config::DEFAULT_BASE_URL.into(),
        );
        let mut embedding = rag.embedding_config().clone();
        embedding.url = format!("{endpoint}/embeddings");
        embedding.model = "test-embeddings".into();
        embedding.api_key_env = None;
        Project {
            id: Uuid::new_v4(),
            path: directory.into(),
            repository: "owner/repo".into(),
            token_env: "AGI_GITHUB_TOKEN".into(),
            sources: vec![".".into()],
            embedding,
        }
    }
    pub(crate) fn test_run(project: &Project, endpoint: &str) -> ReviewRun {
        ReviewRun {
            id: Uuid::new_v4(),
            project_id: project.id,
            repository: project.repository.clone(),
            pr: Some(1),
            head: "abc".into(),
            started: now(),
            status: "running".into(),
            publication: "not_published".into(),
            model: LlmProfile {
                id: "test".into(),
                provider: crate::llm::Provider::Local,
                base_url: endpoint.into(),
                model: "test-model".into(),
                context_tokens: 32768,
                tools: false,
                json_schema: false,
            },
            elapsed_ms: 0,
            usage: None,
            sources: vec![],
            skipped: vec![],
            markdown: String::new(),
            error: None,
        }
    }
    #[test]
    fn persists_projects_and_filters_history_without_chat_database() {
        let directory = std::env::temp_dir().join(format!("agi-review-store-{}", Uuid::new_v4()));
        let store = ReviewStore::at(directory.clone()).unwrap();
        let first = test_project(&directory, "http://localhost:1");
        let mut second = first.clone();
        second.id = Uuid::new_v4();
        second.repository = "owner/other".into();
        store.save_project(&first).unwrap();
        store.save_project(&second).unwrap();
        store
            .save_run(&test_run(&first, "http://localhost:1"))
            .unwrap();
        store
            .save_run(&test_run(&second, "http://localhost:1"))
            .unwrap();
        let reopened = ReviewStore::at(directory.clone()).unwrap();
        assert_eq!(reopened.projects().unwrap().len(), 2);
        assert_eq!(reopened.history(Some(first.id)).unwrap().len(), 1);
        assert_eq!(reopened.history(None).unwrap().len(), 2);
        assert!(!directory.join("chats.sqlite3").exists());
        let workflow = workflow(&first, Path::new("/tmp/a'gi")).unwrap();
        assert!(workflow.contains("AGI_BINARY: '/tmp/a''gi'"));
        assert!(workflow.contains("pull-requests: write"));
        assert!(workflow.contains("Upload review reports"));
        assert!(!workflow.contains("cargo build"));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn accepts_github_remotes_and_rejects_injected_paths() {
        for url in [
            "git@github.com:OlegPrivet/AiAdvent9.git",
            "https://github.com/OlegPrivet/AiAdvent9.git",
            "ssh://git@github.com/OlegPrivet/AiAdvent9.git",
        ] {
            assert_eq!(remote_repository(url).unwrap(), "OlegPrivet/AiAdvent9");
        }
        assert!(remote_repository("https://github.com.evil/foo/bar").is_err());
        assert!(validate_repository("../foo").is_err());
        assert!(validate_sources(&["../secret".into()]).is_err());
    }
}
