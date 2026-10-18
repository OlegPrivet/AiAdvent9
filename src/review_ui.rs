//! Shared review menu, with a responsive TUI adapter and a line-oriented adapter.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};
use std::io::{self, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::input::LineInput;
use crate::review::{Project, Result, ReviewError, ReviewRun, ReviewStore};
use crate::review_github::{Github, PullRequest};
use crate::ui::sanitize_terminal_text;

const AUTOMATION_GUIDE: &str = "# Автоматическое ревью на Mac\n\n1. Выберите общую локальную LLM в /llm и локальные эмбеддинги в карточке проекта.\n2. Для комментариев задайте AGI_GITHUB_TOKEN до запуска agi (Contents: read, Pull requests: read/write). Можно выключить автопубликацию.\n3. /review → проект → Настроить автоматизацию → Включить автоматическое ревью.\n4. Отдельный процесс проверяет открытые PR и новые commit раз в минуту. Включая уже открытые PR; draft пропускаются.\n5. agi можно закрыть: процесс продолжает работу. Mac и ваши серверы должны оставаться включёнными.\n6. Результаты: История проекта. Статус и остановка: Настроить автоматизацию.\n\nRunner, Docker, GitHub Actions и MCP не требуются. Код PR не исполняется; PR из forks читаются через refs/pull основного репозитория. Ошибки повторяются с задержкой, публикация готового отчёта — без повторного LLM-анализа.\n\nПосле перезагрузки Mac запустите процесс снова через UI или agi review automation start. Переменные окружения наследуются при старте процесса; после изменения токена перезапустите его. Журнал: review-watch.log в каталоге состояния agi.";

pub(crate) enum ReviewPage {
    List {
        title: String,
        items: Vec<String>,
    },
    Text {
        title: String,
        value: String,
        multiline: bool,
    },
    Read {
        title: String,
        content: String,
        publish: bool,
    },
    Busy(String),
}
#[derive(Clone, Copy)]
enum Field {
    Path,
    Repository,
    Token,
    Sources,
    EmbeddingUrl,
    EmbeddingModel,
    EmbeddingKey,
    Binary,
    Interval,
}
enum Stage {
    Home,
    Card,
    Field(Field),
    Pulls,
    History,
    Read,
    Workflow,
    Automation,
}
enum JobResult {
    Discovered(PathBuf, String),
    Pulls(Vec<PullRequest>),
    Run(Box<ReviewRun>),
    Published(Box<ReviewRun>),
    Automation(String),
}
pub(crate) struct ReviewManager {
    store: ReviewStore,
    stage: Stage,
    projects: Vec<Project>,
    project: Option<Project>,
    creating: bool,
    history: Vec<ReviewRun>,
    pulls: Vec<PullRequest>,
    report: Option<ReviewRun>,
    read: String,
    workflow: String,
    pub(crate) notice: Option<String>,
    task: Option<JoinHandle<()>>,
    events: mpsc::UnboundedReceiver<std::result::Result<JobResult, String>>,
    sender: mpsc::UnboundedSender<std::result::Result<JobResult, String>>,
    progress: mpsc::UnboundedReceiver<String>,
    progress_sender: mpsc::UnboundedSender<String>,
    status: String,
    llm_label: String,
    workflow_preview: bool,
    automation_read: bool,
    automation: crate::review_watch::Automation,
    daemon_status: String,
    automation_refreshed: std::time::Instant,
}
impl Drop for ReviewManager {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl ReviewManager {
    pub(crate) fn new() -> Result<Self> {
        let mut manager = Self::with_store(ReviewStore::open()?)?;
        let chats =
            crate::chat::ChatStore::open().map_err(|e| ReviewError::Invalid(e.to_string()))?;
        manager.llm_label = chats
            .llms()
            .active()
            .map_err(|e| ReviewError::Invalid(e.to_string()))?
            .label();
        Ok(manager)
    }
    pub(crate) fn with_store(store: ReviewStore) -> Result<Self> {
        let (sender, events) = mpsc::unbounded_channel();
        let (progress_sender, progress) = mpsc::unbounded_channel();
        let projects = store.projects()?;
        Ok(Self {
            store,
            stage: Stage::Home,
            projects,
            project: None,
            creating: false,
            history: Vec::new(),
            pulls: Vec::new(),
            report: None,
            read: String::new(),
            workflow: String::new(),
            notice: None,
            task: None,
            events,
            sender,
            progress,
            progress_sender,
            status: String::new(),
            llm_label: "общая из /llm".into(),
            workflow_preview: false,
            automation_read: false,
            automation: Default::default(),
            daemon_status: String::new(),
            automation_refreshed: std::time::Instant::now(),
        })
    }
    fn project(&self) -> Result<&Project> {
        self.project
            .as_ref()
            .ok_or_else(|| ReviewError::Invalid("Выберите проект".into()))
    }
    fn home(&mut self) -> Result<()> {
        self.projects = self.store.projects()?;
        self.stage = Stage::Home;
        self.project = None;
        self.report = None;
        self.creating = false;
        self.workflow_preview = false;
        self.automation_read = false;
        Ok(())
    }
    pub(crate) fn page(&self) -> ReviewPage {
        if self.task.is_some() {
            return ReviewPage::Busy(self.status.clone());
        }
        match self.stage {
            Stage::Home => ReviewPage::List {
                title: "AI-ревью кода".into(),
                items: std::iter::once("Добавить проект".into())
                    .chain(self.projects.iter().map(|p| p.repository.clone()))
                    .chain([
                        "История всех проектов".into(),
                        "Как работает автоматика".into(),
                        "Вернуться в чат".into(),
                    ])
                    .collect(),
            },
            Stage::Card => {
                let title = self.project.as_ref().map_or_else(
                    || "Проект".into(),
                    |p| {
                        format!(
                            "{}\n{}\nLLM: {} · embeddings: {}\nИсточники: {}",
                            p.repository,
                            p.path.display(),
                            self.llm_label,
                            p.embedding.model,
                            p.sources.join(", ")
                        )
                    },
                );
                ReviewPage::List {
                    title,
                    items: [
                        "Проверить PR",
                        "Проверить локальные изменения",
                        "История проекта",
                        "Настроить автоматизацию",
                        "Изменить GitHub-репозиторий",
                        "Переменная GitHub-токена",
                        "Источники RAG",
                        "URL эмбеддингов",
                        "Модель эмбеддингов",
                        "Переменная ключа эмбеддингов",
                        "Назад",
                    ]
                    .map(str::to_owned)
                    .to_vec(),
                }
            }
            Stage::Field(field) => {
                let p = self.project.as_ref();
                let (title, value, multiline) = match field {
                    Field::Path => (
                        "Каталог Git-проекта",
                        std::env::current_dir()
                            .unwrap_or_default()
                            .display()
                            .to_string(),
                        false,
                    ),
                    Field::Repository => (
                        "GitHub-репозиторий OWNER/REPO",
                        p.map(|p| p.repository.clone()).unwrap_or_default(),
                        false,
                    ),
                    Field::Token => (
                        "Имя переменной GitHub-токена, не сам токен",
                        p.map(|p| p.token_env.clone()).unwrap_or_default(),
                        false,
                    ),
                    Field::Sources => (
                        "Источники RAG: относительный путь на строку; . — весь проект",
                        p.map(|p| p.sources.join("\n")).unwrap_or_default(),
                        true,
                    ),
                    Field::EmbeddingUrl => (
                        "Полный URL /embeddings (сохранение без запросов)",
                        p.map(|p| p.embedding.url.clone()).unwrap_or_default(),
                        false,
                    ),
                    Field::EmbeddingModel => (
                        "Имя модели эмбеддингов",
                        p.map(|p| p.embedding.model.clone()).unwrap_or_default(),
                        false,
                    ),
                    Field::EmbeddingKey => (
                        "Имя переменной ключа эмбеддингов; пусто — без ключа",
                        p.and_then(|p| p.embedding.api_key_env.clone())
                            .unwrap_or_default(),
                        false,
                    ),
                    Field::Interval => (
                        "Интервал проверки PR в секундах (30–3600)",
                        self.automation.interval_secs.to_string(),
                        false,
                    ),
                    Field::Binary => (
                        "Абсолютный путь к установленному доверенному agi",
                        installed_binary().display().to_string(),
                        false,
                    ),
                };
                ReviewPage::Text {
                    title: title.into(),
                    value,
                    multiline,
                }
            }
            Stage::Pulls => ReviewPage::List {
                title: "Открытые PR · выбрать для анализа".into(),
                items: self
                    .pulls
                    .iter()
                    .map(|p| format!("#{} {}", p.number, p.title))
                    .chain(std::iter::once("Назад".into()))
                    .collect(),
            },
            Stage::History => ReviewPage::List {
                title: "История ревью · Enter: открыть · обновляется при открытии".into(),
                items: self
                    .history
                    .iter()
                    .map(|r| {
                        format!(
                            "{} · {} · {} · {}",
                            r.repository,
                            r.pr.map_or_else(|| "локально".into(), |p| format!("#{p}")),
                            status_label(&r.status),
                            r.model.model
                        )
                    })
                    .chain(std::iter::once("Назад".into()))
                    .collect(),
            },
            Stage::Read => ReviewPage::Read {
                title: "Отчёт / инструкция · Esc: назад".into(),
                content: self.read.clone(),
                publish: self
                    .report
                    .as_ref()
                    .is_some_and(|r| r.pr.is_some() && r.status == "completed"),
            },
            Stage::Automation => ReviewPage::List {
                title: format!(
                    "Автоматика: {} · интервал {} с\n{}\nПоследняя проверка: {}\n{}",
                    if self.automation.enabled {
                        "включена"
                    } else {
                        "выключена"
                    },
                    self.automation.interval_secs,
                    self.daemon_status,
                    self.automation.last_poll,
                    self.automation.error.as_deref().unwrap_or("")
                ),
                items: vec![
                    if self.automation.enabled {
                        "Выключить автоматическое ревью"
                    } else {
                        "Включить автоматическое ревью"
                    }
                    .into(),
                    if self.automation.publish {
                        "Выключить автопубликацию (только история)"
                    } else {
                        "Включить автопубликацию комментария в PR"
                    }
                    .into(),
                    "Изменить интервал проверки".into(),
                    "Запустить фоновый процесс".into(),
                    "Остановить фоновый процесс (все проекты)".into(),
                    "Показать подробный статус".into(),
                    "Экспорт GitHub Action (дополнительно)".into(),
                    "Повторить ошибки сейчас".into(),
                    "Назад".into(),
                ],
            },
            Stage::Workflow => ReviewPage::List {
                title: "Workflow готов к сохранению".into(),
                items: vec![
                    "Предварительный просмотр".into(),
                    "Сохранить .github/workflows/ai-review.yml (заменить существующий)".into(),
                    "Отмена".into(),
                ],
            },
        }
    }
    pub(crate) fn select(&mut self, index: usize) -> Result<bool> {
        self.notice = None;
        if let ReviewPage::List { items, .. } = self.page()
            && index >= items.len()
        {
            return Err(ReviewError::Invalid("Нет такого пункта меню".into()));
        }
        match self.stage {
            Stage::Home => {
                if index == 0 {
                    self.creating = true;
                    self.stage = Stage::Field(Field::Path);
                } else if let Some(project) = self.projects.get(index - 1).cloned() {
                    self.project = Some(project);
                    self.stage = Stage::Card;
                } else if index == self.projects.len() + 1 {
                    self.history = self.store.history(None)?;
                    self.stage = Stage::History;
                } else if index == self.projects.len() + 2 {
                    self.read = AUTOMATION_GUIDE.into();
                    self.report = None;
                    self.stage = Stage::Read;
                } else {
                    return Ok(true);
                }
            }
            Stage::Card => match index {
                0 => self.load_pulls()?,
                1 => self.start_review(None)?,
                2 => {
                    self.history = self.store.history(Some(self.project()?.id))?;
                    self.stage = Stage::History;
                }
                3 => {
                    self.stage = Stage::Automation;
                    self.refresh_automation()?;
                }
                4 => self.stage = Stage::Field(Field::Repository),
                5 => self.stage = Stage::Field(Field::Token),
                6 => self.stage = Stage::Field(Field::Sources),
                7 => self.stage = Stage::Field(Field::EmbeddingUrl),
                8 => self.stage = Stage::Field(Field::EmbeddingModel),
                9 => self.stage = Stage::Field(Field::EmbeddingKey),
                _ => self.home()?,
            },
            Stage::Pulls => {
                if let Some(pull) = self.pulls.get(index) {
                    self.start_review(Some(pull.number))?;
                } else {
                    self.stage = Stage::Card;
                }
            }
            Stage::History => {
                if let Some(run) = self.history.get(index).cloned() {
                    self.read = report_content(&run);
                    self.report = Some(run);
                    self.stage = Stage::Read;
                } else {
                    self.back()?;
                }
            }
            Stage::Automation => match index {
                0 => {
                    let mut settings = self.automation.clone();
                    settings.enabled = !settings.enabled;
                    if settings.enabled {
                        crate::review_watch::validate_setup(
                            &self.store,
                            self.project()?,
                            &settings,
                        )?;
                    }
                    self.store.save_automation(self.project()?.id, &settings)?;
                    if settings.enabled {
                        self.start_automation();
                    } else {
                        self.notice = Some(
                            "Автоматика проекта выключена; текущий анализ будет отменён".into(),
                        );
                    }
                    self.refresh_automation()?;
                }
                1 => {
                    let mut settings = self.automation.clone();
                    settings.publish = !settings.publish;
                    if settings.enabled {
                        crate::review_watch::validate_setup(
                            &self.store,
                            self.project()?,
                            &settings,
                        )?;
                    }
                    self.store.save_automation(self.project()?.id, &settings)?;
                    self.refresh_automation()?;
                }
                2 => self.stage = Stage::Field(Field::Interval),
                3 => self.start_automation(),
                4 => {
                    self.notice = Some(crate::review_watch::stop(&self.store)?);
                    self.refresh_automation()?;
                }
                5 => {
                    self.refresh_automation()?;
                    self.read = format!(
                        "# Автоматическое ревью\n\n{}\n\nПроект: {}\nID проекта: {}\nВключено: {}\nАвтопубликация: {}\nИнтервал: {} с\nПоследняя проверка: {}\nОшибка: {}\n\nЖурнал: {}\n\nПосле изменения переменных окружения остановите процесс и запустите заново из agi, который получил новые значения.",
                        self.daemon_status,
                        self.project()?.repository,
                        self.project()?.id,
                        self.automation.enabled,
                        self.automation.publish,
                        self.automation.interval_secs,
                        self.automation.last_poll,
                        self.automation.error.as_deref().unwrap_or("нет"),
                        self.store.directory.join("review-watch.log").display()
                    );
                    self.report = None;
                    self.automation_read = true;
                    self.stage = Stage::Read;
                }
                6 => self.stage = Stage::Field(Field::Binary),
                7 => {
                    self.store.retry_automation(self.project()?.id)?;
                    self.notice = Some("Ошибки будут повторены при следующей проверке".into());
                    self.refresh_automation()?;
                }
                _ => self.stage = Stage::Card,
            },
            Stage::Workflow => match index {
                0 => {
                    self.workflow_preview = true;
                    self.read = self.workflow.clone();
                    self.report = None;
                    self.stage = Stage::Read;
                }
                1 => {
                    let path = self.project()?.path.join(".github/workflows/ai-review.yml");
                    let parent = path
                        .parent()
                        .ok_or_else(|| ReviewError::Invalid("Нет каталога workflow".into()))?;
                    // Refuse writes through symlinked .github/workflows directories.
                    for relative in [
                        ".github",
                        ".github/workflows",
                        ".github/workflows/ai-review.yml",
                    ] {
                        if std::fs::symlink_metadata(self.project()?.path.join(relative))
                            .is_ok_and(|m| m.file_type().is_symlink())
                        {
                            return Err(ReviewError::Invalid(
                                "Путь workflow содержит symlink".into(),
                            ));
                        }
                    }
                    std::fs::create_dir_all(parent)?;
                    std::fs::write(&path, &self.workflow)?;
                    self.notice = Some(format!(
                        "Сохранено {}. Добавьте файл в Git и отправьте на GitHub самостоятельно.",
                        path.display()
                    ));
                    self.stage = Stage::Card;
                }
                _ => self.stage = Stage::Card,
            },
            _ => {}
        }
        Ok(false)
    }
    pub(crate) fn submit(&mut self, value: String) -> Result<()> {
        let Stage::Field(field) = self.stage else {
            return Ok(());
        };
        let value = value.trim().to_owned();
        if matches!(field, Field::Path) {
            let path = if let Some(tail) = value.strip_prefix("~/") {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(tail)
            } else {
                PathBuf::from(value)
            };
            let sender = self.sender.clone();
            self.status = "Определение репозитория".into();
            self.task = Some(tokio::spawn(async move {
                let result = crate::review_git::discover(&path)
                    .await
                    .map(|(p, r)| JobResult::Discovered(p, r))
                    .map_err(|e| e.to_string());
                let _ = sender.send(result);
            }));
            return Ok(());
        }
        if matches!(field, Field::Interval) {
            let mut settings = self.automation.clone();
            settings.interval_secs = value
                .parse()
                .map_err(|_| ReviewError::Invalid("Введите число секунд от 30 до 3600".into()))?;
            self.store.save_automation(self.project()?.id, &settings)?;
            self.stage = Stage::Automation;
            self.refresh_automation()?;
            return Ok(());
        }
        if matches!(field, Field::Binary) {
            let path = PathBuf::from(value);
            if !path.is_absolute() || !path.is_file() {
                return Err(ReviewError::Invalid(
                    "Укажите абсолютный путь к установленному agi (cargo install --path .)".into(),
                ));
            }
            self.workflow = crate::review::workflow(self.project()?, &path)?;
            self.stage = Stage::Workflow;
            return Ok(());
        }
        let mut project = self.project()?.clone();
        match field {
            Field::Repository => {
                crate::review::validate_repository(&value)?;
                project.repository = value;
            }
            Field::Token => {
                crate::review::validate_env(&value)?;
                project.token_env = value;
            }
            Field::Sources => {
                project.sources = value
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                crate::review::validate_sources(&project.sources)?;
            }
            Field::EmbeddingUrl => project.embedding.url = value,
            Field::EmbeddingModel => project.embedding.model = value,
            Field::EmbeddingKey => {
                if !value.is_empty() {
                    crate::review::validate_env(&value)?;
                }
                project.embedding.api_key_env = (!value.is_empty()).then_some(value);
            }
            _ => {}
        }
        crate::rag::validate_embedding_options(
            &project.embedding.url,
            &project.embedding.model,
            project.embedding.api_key_env.as_deref(),
        )?;
        self.store.save_project(&project)?;
        self.project = Some(project);
        self.creating = false;
        self.stage = Stage::Card;
        Ok(())
    }
    fn refresh_automation(&mut self) -> Result<()> {
        self.automation = self.store.automation(self.project()?.id)?;
        self.daemon_status = crate::review_watch::status(&self.store)?.label();
        self.automation_refreshed = std::time::Instant::now();
        Ok(())
    }
    fn start_automation(&mut self) {
        let store = self.store.clone();
        let sender = self.sender.clone();
        self.status = "Запуск отдельного фонового процесса".into();
        self.task = Some(tokio::spawn(async move {
            let result = crate::review_watch::start(&store)
                .await
                .map(JobResult::Automation)
                .map_err(|e| e.to_string());
            let _ = sender.send(result);
        }));
    }
    fn load_pulls(&mut self) -> Result<()> {
        let github = Github::new(self.project()?)?;
        let sender = self.sender.clone();
        self.status = "Загрузка открытых PR".into();
        self.task = Some(tokio::spawn(async move {
            let _ = sender.send(
                github
                    .pulls()
                    .await
                    .map(JobResult::Pulls)
                    .map_err(|e| e.to_string()),
            );
        }));
        Ok(())
    }
    fn start_review(&mut self, pr: Option<u64>) -> Result<()> {
        let project = self.project()?.clone();
        let chats =
            crate::chat::ChatStore::open().map_err(|e| ReviewError::Invalid(e.to_string()))?;
        let model = chats
            .llms()
            .active()
            .map_err(|e| ReviewError::Invalid(e.to_string()))?;
        let store = self.store.clone();
        let sender = self.sender.clone();
        let progress = self.progress_sender.clone();
        self.status = format!("Начало ревью · {}", model.model);
        self.task = Some(tokio::spawn(async move {
            let result = crate::review_pipeline::run(&store, project, model, pr, None, |stage| {
                let _ = progress.send(stage.into());
            })
            .await;
            let _ = sender.send(
                result
                    .map(|r| JobResult::Run(Box::new(r)))
                    .map_err(|e| e.to_string()),
            );
        }));
        Ok(())
    }
    pub(crate) fn publish(&mut self) -> Result<()> {
        let mut run = self
            .report
            .clone()
            .ok_or_else(|| ReviewError::Invalid("Нет отчёта".into()))?;
        let project = self.store.project(run.project_id)?;
        if run.pr.is_none() || run.status != "completed" {
            return Err(ReviewError::Invalid(
                "Этот отчёт нельзя опубликовать".into(),
            ));
        }
        let store = self.store.clone();
        let sender = self.sender.clone();
        self.status = "Публикация комментария в PR".into();
        self.task = Some(tokio::spawn(async move {
            let result = crate::review_github::publish(&store, &project, &mut run)
                .await
                .map(|()| JobResult::Published(Box::new(run)))
                .map_err(|e| e.to_string());
            let _ = sender.send(result);
        }));
        Ok(())
    }
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        if matches!(self.stage, Stage::Automation)
            && self.task.is_none()
            && self.automation_refreshed.elapsed().as_secs() >= crate::config::REVIEW_HEARTBEAT_SECS
        {
            if let Err(error) = self.refresh_automation() {
                self.notice = Some(error.to_string());
            }
            changed = true;
        }
        while let Ok(progress) = self.progress.try_recv() {
            self.status = progress;
            changed = true;
        }
        while let Ok(result) = self.events.try_recv() {
            changed = true;
            self.task.take();
            let result: Result<()> = match result {
                Err(error) => {
                    self.notice = Some(error);
                    Ok(())
                }
                Ok(JobResult::Automation(message)) => {
                    self.notice = Some(message);
                    self.stage = Stage::Automation;
                    self.refresh_automation()
                }
                Ok(JobResult::Discovered(path, repository)) => (|| {
                    let rag = crate::rag::RagService::open(
                        std::env::var("NEURALDEEP_API_KEY").ok(),
                        crate::config::DEFAULT_BASE_URL.into(),
                    )?;
                    self.project = Some(Project {
                        id: uuid::Uuid::new_v4(),
                        path,
                        repository,
                        token_env: "AGI_GITHUB_TOKEN".into(),
                        sources: vec![".".into()],
                        embedding: rag.embedding_config().clone(),
                    });
                    self.stage = Stage::Field(Field::Repository);
                    Ok(())
                })(),
                Ok(JobResult::Pulls(pulls)) => {
                    self.pulls = pulls;
                    self.stage = Stage::Pulls;
                    Ok(())
                }
                Ok(JobResult::Run(run) | JobResult::Published(run)) => {
                    self.read = report_content(&run);
                    self.report = Some(*run);
                    self.stage = Stage::Read;
                    Ok(())
                }
            };
            if let Err(error) = result {
                self.notice = Some(error.to_string());
            }
        }
        changed
    }
    fn back(&mut self) -> Result<()> {
        self.report = None;
        if std::mem::take(&mut self.automation_read) {
            self.stage = Stage::Automation;
            self.refresh_automation()?;
            return Ok(());
        }
        if std::mem::take(&mut self.workflow_preview) {
            self.stage = Stage::Workflow;
            return Ok(());
        }
        if self.project.is_some() && !self.creating {
            self.stage = Stage::Card;
        } else {
            self.home()?;
        }
        Ok(())
    }
    pub(crate) fn cancel(&mut self) -> Result<bool> {
        if let Some(task) = self.task.take() {
            task.abort();
            self.notice = Some("Операция отменена".into());
            return Ok(false);
        }
        if matches!(self.stage, Stage::Home) {
            return Ok(true);
        }
        self.back()?;
        Ok(false)
    }
}
fn installed_binary() -> PathBuf {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cargo")
        })
        .join("bin/agi")
}
fn status_label(status: &str) -> &str {
    match status {
        "completed" => "готово",
        "failed" => "ошибка",
        "cancelled" => "отменено",
        _ => "выполняется / прервано",
    }
}
fn report_content(run: &ReviewRun) -> String {
    format!(
        "{}\n\nСтатус: {} · публикация: {}\nМодель: {} · время: {} мс · токены: {}\n{}",
        run.markdown,
        status_label(&run.status),
        run.publication,
        run.model.model,
        run.elapsed_ms,
        run.usage
            .map_or_else(|| "нет данных".into(), |u| u.total_tokens.to_string()),
        run.error.as_deref().unwrap_or("")
    )
}

pub(crate) struct ReviewModal {
    pub(crate) manager: ReviewManager,
    pub(crate) input: TextArea<'static>,
    selected: usize,
    scroll: u16,
}
impl ReviewModal {
    pub(crate) fn new(manager: ReviewManager) -> Self {
        let mut modal = Self {
            manager,
            input: TextArea::default(),
            selected: 0,
            scroll: 0,
        };
        modal.refresh();
        modal
    }
    fn refresh(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        if let ReviewPage::Text { value, .. } = self.manager.page() {
            self.input = TextArea::new(
                sanitize_terminal_text(&value)
                    .split('\n')
                    .map(str::to_owned)
                    .collect(),
            );
            self.input.move_cursor(CursorMove::Top);
            self.input.move_cursor(CursorMove::Head);
            self.input.start_selection();
            self.input.move_cursor(CursorMove::Bottom);
            self.input.move_cursor(CursorMove::End);
        }
    }
    pub(crate) fn poll(&mut self) -> bool {
        let was_busy = matches!(self.manager.page(), ReviewPage::Busy(_));
        let changed = self.manager.poll();
        if changed && was_busy && !matches!(self.manager.page(), ReviewPage::Busy(_)) {
            self.refresh();
        }
        changed
    }
    pub(crate) fn handle_mouse(&mut self, kind: MouseEventKind) {
        if matches!(self.manager.page(), ReviewPage::Read { .. }) {
            match kind {
                MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(3),
                _ => {}
            }
        }
    }
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        let control_cancel =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        let result = if key.code == KeyCode::Esc || control_cancel {
            Some(self.manager.cancel())
        } else {
            match self.manager.page() {
                ReviewPage::List { items, .. } => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.selected = self.selected.saturating_sub(1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.selected = (self.selected + 1).min(items.len().saturating_sub(1));
                        None
                    }
                    KeyCode::Enter => Some(self.manager.select(self.selected)),
                    _ => None,
                },
                ReviewPage::Text { multiline, .. } => {
                    if key.code == KeyCode::F(4)
                        || key.code == KeyCode::Enter
                            && (!multiline || key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        Some(
                            self.manager
                                .submit(self.input.lines().join("\n"))
                                .map(|()| false),
                        )
                    } else {
                        self.input.input(key);
                        None
                    }
                }
                ReviewPage::Read { publish, .. } => match key.code {
                    KeyCode::Char('p') if publish => Some(self.manager.publish().map(|()| false)),
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.scroll = self.scroll.saturating_sub(1);
                        None
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.scroll = self.scroll.saturating_add(1);
                        None
                    }
                    KeyCode::PageUp => {
                        self.scroll = self.scroll.saturating_sub(10);
                        None
                    }
                    KeyCode::PageDown => {
                        self.scroll = self.scroll.saturating_add(10);
                        None
                    }
                    KeyCode::Home => {
                        self.scroll = 0;
                        None
                    }
                    KeyCode::End => {
                        self.scroll = u16::MAX;
                        None
                    }
                    _ => None,
                },
                ReviewPage::Busy(_) => None,
            }
        };
        match result {
            Some(Ok(true)) => true,
            Some(Ok(false)) => {
                self.refresh();
                false
            }
            Some(Err(error)) => {
                self.manager.notice = Some(error.to_string());
                false
            }
            None => false,
        }
    }
    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let title = self
            .manager
            .notice
            .as_deref()
            .map(sanitize_terminal_text)
            .unwrap_or_else(|| "AI-ревью · Esc: назад / отменить".into());
        match self.manager.page() {
            ReviewPage::List {
                title: details,
                items,
            } => {
                let rows = ratatui::layout::Layout::vertical([
                    ratatui::layout::Constraint::Length(5),
                    ratatui::layout::Constraint::Min(1),
                ])
                .split(area);
                frame.render_widget(
                    Paragraph::new(sanitize_terminal_text(&details)).wrap(Wrap { trim: false }),
                    rows[0],
                );
                let list = List::new(
                    items
                        .into_iter()
                        .map(|s| ListItem::new(sanitize_terminal_text(&s)))
                        .collect::<Vec<_>>(),
                )
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(Style::default().fg(Color::Cyan))
                .highlight_symbol("› ");
                frame.render_stateful_widget(
                    list,
                    rows[1],
                    &mut ListState::default().with_selected(Some(self.selected)),
                );
            }
            ReviewPage::Text {
                title: details,
                multiline,
                ..
            } => {
                self.input.set_block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(sanitize_terminal_text(&details))
                        .title_bottom(if multiline {
                            "Ctrl+Enter/F4: сохранить · Esc: назад"
                        } else {
                            "Enter: сохранить · Esc: назад"
                        }),
                );
                frame.render_widget(&self.input, area);
                if self.manager.notice.is_some() {
                    frame.render_widget(
                        Paragraph::new(title).style(Style::default().fg(Color::Yellow)),
                        Rect { height: 1, ..area },
                    );
                }
            }
            ReviewPage::Read {
                title: details,
                content,
                publish,
            } => {
                let text = ratatui::text::Text::from(sanitize_terminal_text(&content));
                let markdown = text.to_string();
                let text = tui_markdown::from_str(&markdown);
                // Markdown parser returns owned text; sanitize before parsing untrusted model output.
                let max = crate::tui::wrapped_text_height(&text, area.width.saturating_sub(2))
                    .saturating_sub(usize::from(area.height.saturating_sub(2)))
                    .min(u16::MAX as usize) as u16;
                self.scroll = self.scroll.min(max);
                frame.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((self.scroll, 0))
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(details)
                                .title_bottom(if publish {
                                    "p: опубликовать в PR · ↑/↓ PgUp/PgDn: прокрутка"
                                } else {
                                    "↑/↓ PgUp/PgDn: прокрутка · Esc: назад"
                                }),
                        ),
                    area,
                );
            }
            ReviewPage::Busy(stage) => {
                frame.render_widget(
                    Paragraph::new(sanitize_terminal_text(&stage))
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title("Ревью выполняется · Esc/Ctrl+C: отменить"),
                        ),
                    area,
                );
            }
        }
    }
}

pub(crate) async fn run<I: LineInput, W: Write>(input: &mut I, output: &mut W) -> io::Result<()> {
    let mut manager = ReviewManager::new().map_err(io::Error::other)?;
    loop {
        manager.poll();
        if let Some(notice) = manager.notice.take() {
            writeln!(output, "{}", sanitize_terminal_text(&notice))?;
        }
        let result = match manager.page() {
            ReviewPage::Busy(stage) => {
                writeln!(output, "{}", sanitize_terminal_text(&stage))?;
                output.flush()?;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
                    _ = tokio::signal::ctrl_c() => { manager.cancel().map_err(io::Error::other)?; },
                }
                continue;
            }
            ReviewPage::List { title, items } => {
                writeln!(output, "{}", sanitize_terminal_text(&title))?;
                for (i, item) in items.iter().enumerate() {
                    writeln!(output, "{}. {}", i + 1, sanitize_terminal_text(item))?;
                }
                let Some(value) = input.read_line("review · номер / назад")? else {
                    return Ok(());
                };
                if value.trim().is_empty() {
                    manager.cancel()
                } else if let Ok(index) = value.trim().parse::<usize>() {
                    manager.select(index.saturating_sub(1))
                } else {
                    Err(ReviewError::Invalid("Введите номер пункта".into()))
                }
            }
            ReviewPage::Text {
                title,
                value,
                multiline,
            } => {
                writeln!(
                    output,
                    "{}\nТекущее значение: {}",
                    sanitize_terminal_text(&title),
                    sanitize_terminal_text(&value)
                )?;
                let Some(value) = input.read_line(if multiline {
                    "пути через запятую / назад"
                } else {
                    "значение / назад"
                })?
                else {
                    return Ok(());
                };
                if value == "назад" {
                    manager.cancel()
                } else {
                    manager
                        .submit(if multiline {
                            value
                                .split(',')
                                .map(str::trim)
                                .collect::<Vec<_>>()
                                .join("\n")
                        } else {
                            value
                        })
                        .map(|()| false)
                }
            }
            ReviewPage::Read {
                content, publish, ..
            } => {
                writeln!(output, "{}", sanitize_terminal_text(&content))?;
                let Some(value) = input.read_line(if publish {
                    "p: опубликовать / Enter: назад"
                } else {
                    "Enter: назад"
                })?
                else {
                    return Ok(());
                };
                if publish && value.trim() == "p" {
                    manager.publish().map(|()| false)
                } else {
                    manager.cancel()
                }
            }
        };
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => manager.notice = Some(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_project_settings_offline_and_previews_before_saving_workflow() {
        let directory =
            std::env::temp_dir().join(format!("agi-review-ui-{}", uuid::Uuid::new_v4()));
        let store = ReviewStore::at(directory.clone()).unwrap();
        let project = crate::review::tests::test_project(&directory, "http://127.0.0.1:1");
        store.save_project(&project).unwrap();
        let mut manager = ReviewManager::with_store(store.clone()).unwrap();
        manager.select(1).unwrap();
        manager.select(6).unwrap();
        manager.submit("src\nREADME.md".into()).unwrap();
        assert_eq!(
            store.project(project.id).unwrap().sources,
            ["src", "README.md"]
        );
        manager.select(7).unwrap();
        manager
            .submit("http://127.0.0.1:2/embeddings".into())
            .unwrap();
        assert_eq!(
            store.project(project.id).unwrap().embedding.url,
            "http://127.0.0.1:2/embeddings"
        );
        let binary = directory.join("agi");
        std::fs::write(&binary, "fixture").unwrap();
        manager.select(3).unwrap();
        manager.select(6).unwrap();
        manager.submit(binary.display().to_string()).unwrap();
        assert!(!directory.join(".github").exists());
        assert!(matches!(manager.page(), ReviewPage::List { .. }));
        manager.select(0).unwrap();
        assert!(matches!(
            manager.page(),
            ReviewPage::Read { publish: false, .. }
        ));
        manager.cancel().unwrap();
        manager.select(1).unwrap();
        assert!(directory.join(".github/workflows/ai-review.yml").is_file());
        let mut modal = ReviewModal::new(manager);
        let backend = ratatui::backend::TestBackend::new(100, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| modal.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("Проверить PR"));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn automation_settings_persist_and_status_refresh_preserves_menu_selection() {
        let directory = std::env::temp_dir().join(format!("agi-watch-ui-{}", uuid::Uuid::new_v4()));
        let store = ReviewStore::at(directory.clone()).unwrap();
        let project = crate::review::tests::test_project(&directory, "http://127.0.0.1:1");
        store.save_project(&project).unwrap();
        let mut manager = ReviewManager::with_store(store.clone()).unwrap();
        manager.select(1).unwrap();
        manager.select(3).unwrap();
        manager.select(1).unwrap(); // Configure local-only reports, still disabled.
        manager.select(2).unwrap();
        manager.submit("45".into()).unwrap();
        assert!(!store.automation(project.id).unwrap().publish);
        assert_eq!(store.automation(project.id).unwrap().interval_secs, 45);
        let mut modal = ReviewModal::new(manager);
        modal.selected = 4;
        modal.manager.automation_refreshed =
            std::time::Instant::now() - std::time::Duration::from_secs(3);
        assert!(modal.poll());
        assert_eq!(modal.selected, 4);
        modal.manager.select(5).unwrap();
        assert!(matches!(
            modal.manager.page(),
            ReviewPage::Read { publish: false, .. }
        ));
        modal.manager.cancel().unwrap();
        assert!(matches!(modal.manager.stage, Stage::Automation));
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn modal_cancellation_aborts_background_work_without_closing_menu() {
        let directory =
            std::env::temp_dir().join(format!("agi-review-ui-cancel-{}", uuid::Uuid::new_v4()));
        let store = ReviewStore::at(directory.clone()).unwrap();
        let mut manager = ReviewManager::with_store(store).unwrap();
        manager.task = Some(tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }));
        let mut modal = ReviewModal::new(manager);
        assert!(!modal.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(modal.manager.task.is_none());
        assert!(matches!(modal.manager.page(), ReviewPage::List { .. }));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
