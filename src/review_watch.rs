//! Local PR automation. GitHub data never becomes commands or workflows on this host.
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::Subcommand;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config;
use crate::review::{Project, Result, ReviewError, ReviewRun, ReviewStore, now, private};
use crate::review_github::{Github, PullRequest};

#[derive(Debug, Clone)]
pub(crate) struct Automation {
    pub(crate) enabled: bool,
    pub(crate) publish: bool,
    pub(crate) interval_secs: u64,
    pub(crate) last_poll: u64,
    pub(crate) error: Option<String>,
}
impl Default for Automation {
    fn default() -> Self {
        Self {
            enabled: false,
            publish: true,
            interval_secs: config::REVIEW_POLL_SECS,
            last_poll: 0,
            error: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Checkpoint {
    project: Uuid,
    repository: String,
    pr: u64,
    head: String,
    run: Option<Uuid>,
    attempts: u32,
    retry_at: u64,
    status: String,
    error: Option<String>,
}
#[derive(Default)]
pub(crate) struct DaemonStatus {
    pub(crate) running: bool,
    pub(crate) pid: u32,
    pub(crate) heartbeat: u64,
    pub(crate) stopping: bool,
    pub(crate) stage: String,
}
impl DaemonStatus {
    pub(crate) fn label(&self) -> String {
        if !self.running {
            return "Фоновый процесс остановлен".into();
        }
        format!(
            "{} · PID {} · связь {} с назад\n{}",
            if self.stopping {
                "Останавливается"
            } else {
                "Фоновый процесс работает"
            },
            self.pid,
            now().saturating_sub(self.heartbeat),
            self.stage
        )
    }
}

impl ReviewStore {
    fn watch_schema(&self) -> Result<()> {
        self.connection()?.execute_batch("CREATE TABLE IF NOT EXISTS review_automation(
            project_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL, publish INTEGER NOT NULL,
            interval_secs INTEGER NOT NULL, last_poll INTEGER NOT NULL DEFAULT 0, error TEXT);
            CREATE TABLE IF NOT EXISTS review_checkpoints(
            project_id TEXT NOT NULL, repository TEXT NOT NULL, pr INTEGER NOT NULL, head TEXT NOT NULL,
            data TEXT NOT NULL, PRIMARY KEY(project_id,repository,pr,head));
            CREATE TABLE IF NOT EXISTS review_daemon(
            id INTEGER PRIMARY KEY CHECK(id=1), token TEXT NOT NULL, pid INTEGER NOT NULL,
            heartbeat INTEGER NOT NULL, stop INTEGER NOT NULL, stage TEXT NOT NULL);")?;
        Ok(())
    }
    pub(crate) fn automation(&self, project: Uuid) -> Result<Automation> {
        self.watch_schema()?;
        Ok(self.connection()?.query_row("SELECT enabled,publish,interval_secs,last_poll,error FROM review_automation WHERE project_id=?1", [project.to_string()], |row| Ok(Automation {
            enabled: row.get(0)?, publish: row.get(1)?, interval_secs: row.get::<_, i64>(2)? as u64,
            last_poll: row.get::<_, i64>(3)? as u64, error: row.get(4)?,
        })).optional()?.unwrap_or_default())
    }
    pub(crate) fn save_automation(&self, project: Uuid, settings: &Automation) -> Result<()> {
        self.project(project)?;
        if !(30..=3600).contains(&settings.interval_secs) {
            return Err(ReviewError::Invalid(
                "Интервал проверки: от 30 до 3600 секунд".into(),
            ));
        }
        self.watch_schema()?;
        self.connection()?.execute("INSERT INTO review_automation(project_id,enabled,publish,interval_secs) VALUES(?1,?2,?3,?4)
            ON CONFLICT(project_id) DO UPDATE SET enabled=excluded.enabled,publish=excluded.publish,interval_secs=excluded.interval_secs,last_poll=0,error=NULL",
            params![project.to_string(), settings.enabled, settings.publish, settings.interval_secs as i64])?;
        Ok(())
    }
    fn polled(&self, project: Uuid, error: Option<&str>) -> Result<()> {
        self.connection()?.execute(
            "UPDATE review_automation SET last_poll=?2,error=?3 WHERE project_id=?1",
            params![project.to_string(), now() as i64, error],
        )?;
        Ok(())
    }
    fn checkpoint(&self, project: &Project, pull: &PullRequest) -> Result<Checkpoint> {
        self.watch_schema()?;
        let data: Option<String> = self.connection()?.query_row("SELECT data FROM review_checkpoints WHERE project_id=?1 AND repository=?2 AND pr=?3 AND head=?4",
            params![project.id.to_string(), project.repository, pull.number as i64, pull.head.sha], |row| row.get(0)).optional()?;
        data.map(|data| serde_json::from_str(&data).map_err(Into::into))
            .unwrap_or_else(|| {
                Ok(Checkpoint {
                    project: project.id,
                    repository: project.repository.clone(),
                    pr: pull.number,
                    head: pull.head.sha.clone(),
                    run: None,
                    attempts: 0,
                    retry_at: 0,
                    status: "pending".into(),
                    error: None,
                })
            })
    }
    fn save_checkpoint(&self, checkpoint: &Checkpoint) -> Result<()> {
        self.connection()?.execute("INSERT INTO review_checkpoints VALUES(?1,?2,?3,?4,?5) ON CONFLICT(project_id,repository,pr,head) DO UPDATE SET data=excluded.data",
            params![checkpoint.project.to_string(), checkpoint.repository, checkpoint.pr as i64, checkpoint.head, serde_json::to_string(checkpoint)?])?;
        Ok(())
    }
    fn saved_run(&self, id: Uuid) -> Result<Option<ReviewRun>> {
        let data: Option<String> = self
            .connection()?
            .query_row(
                "SELECT run_json FROM review_runs WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|data| serde_json::from_str(&data).map_err(Into::into))
            .transpose()
    }
    pub(crate) fn retry_automation(&self, project: Uuid) -> Result<()> {
        self.watch_schema()?;
        self.connection()?.execute("UPDATE review_checkpoints SET data=json_set(data,'$.retry_at',0) WHERE project_id=?1 AND json_extract(data,'$.status')='failed'", [project.to_string()])?;
        self.connection()?.execute(
            "UPDATE review_automation SET last_poll=0,error=NULL WHERE project_id=?1",
            [project.to_string()],
        )?;
        Ok(())
    }
}

fn lock_file(store: &ReviewStore) -> Result<File> {
    let path = store.directory.join("review-watch.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    private(&path, 0o600)?;
    Ok(file)
}
fn acquire(store: &ReviewStore) -> Result<Option<File>> {
    let file = lock_file(store)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}
pub(crate) fn status(store: &ReviewStore) -> Result<DaemonStatus> {
    store.watch_schema()?;
    let running = acquire(store)?.is_none();
    let mut result = store
        .connection()?
        .query_row(
            "SELECT pid,heartbeat,stop,stage FROM review_daemon WHERE id=1",
            [],
            |row| {
                Ok(DaemonStatus {
                    running,
                    pid: row.get::<_, i64>(0)? as u32,
                    heartbeat: row.get::<_, i64>(1)? as u64,
                    stopping: row.get(2)?,
                    stage: row.get(3)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default();
    result.running = running;
    Ok(result)
}

pub(crate) async fn start(store: &ReviewStore) -> Result<String> {
    start_with_binary(store, &std::env::current_exe()?).await
}
async fn start_with_binary(store: &ReviewStore, binary: &Path) -> Result<String> {
    if status(store)?.running {
        return Ok("Фоновый процесс уже работает".into());
    }
    let path = store.directory.join("review-watch.log");
    let log = OpenOptions::new().create(true).append(true).open(&path)?;
    private(&path, 0o600)?;
    let mut command = Command::new(binary);
    command
        .args(["review", "watch", "--state-dir"])
        .arg(&store.directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .env_remove("GITHUB_ACTIONS")
        .env_remove("GITHUB_TOKEN");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x08000000);
    }
    let mut child = command.spawn()?;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if status(store)?.running {
            // Reap the child eventually if this UI remains open. Dropping this task doesn't kill it.
            std::thread::Builder::new()
                .name("agi-review-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                })?;
            return Ok("Фоновая проверка запущена; agi можно закрыть".into());
        }
        if let Some(exit) = child.try_wait()? {
            return Err(ReviewError::Invalid(format!(
                "Фоновый процесс завершился ({exit}); журнал: {}",
                path.display()
            )));
        }
    }
    // Spawn succeeded, so do not kill a process that may still be starting.
    std::thread::Builder::new()
        .name("agi-review-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(format!(
        "Процесс запускается; проверьте статус. Журнал: {}",
        path.display()
    ))
}
pub(crate) fn stop(store: &ReviewStore) -> Result<String> {
    if !status(store)?.running {
        return Ok("Фоновый процесс уже остановлен".into());
    }
    store
        .connection()?
        .execute("UPDATE review_daemon SET stop=1 WHERE id=1", [])?;
    Ok("Остановка запрошена для всех проектов; текущий анализ будет отменён".into())
}

#[derive(Debug, Subcommand)]
pub(crate) enum ControlCommand {
    Start,
    Stop,
    Status,
    /// Включить проект; по умолчанию публиковать комментарий.
    Enable {
        #[arg(long)]
        project: Uuid,
        #[arg(long)]
        local_only: bool,
    },
    Disable {
        #[arg(long)]
        project: Uuid,
    },
    Retry {
        #[arg(long)]
        project: Uuid,
    },
}
pub(crate) async fn control(store: &ReviewStore, command: ControlCommand) -> Result<()> {
    let message = match command {
        ControlCommand::Start => start(store).await?,
        ControlCommand::Stop => stop(store)?,
        ControlCommand::Status => status(store)?.label(),
        ControlCommand::Enable {
            project,
            local_only,
        } => {
            let definition = store.project(project)?;
            let mut settings = store.automation(project)?;
            settings.enabled = true;
            settings.publish = !local_only;
            validate_setup(store, &definition, &settings)?;
            store.save_automation(project, &settings)?;
            start(store).await?
        }
        ControlCommand::Disable { project } => {
            let mut settings = store.automation(project)?;
            settings.enabled = false;
            store.save_automation(project, &settings)?;
            "Автоматика проекта выключена".into()
        }
        ControlCommand::Retry { project } => {
            store.retry_automation(project)?;
            "Ошибки будут повторены при следующей проверке".into()
        }
    };
    println!("{message}");
    Ok(())
}
pub(crate) fn validate_setup(
    store: &ReviewStore,
    project: &Project,
    settings: &Automation,
) -> Result<()> {
    let model = crate::chat::ChatStore::with_directory(store.directory.clone())
        .map_err(|e| ReviewError::Invalid(e.to_string()))?
        .llms()
        .active()
        .map_err(|e| ReviewError::Invalid(e.to_string()))?;
    validate_setup_with_env(&model, project, settings, |name| std::env::var(name).ok())
}

fn validate_setup_with_env(
    model: &crate::llm::LlmProfile,
    project: &Project,
    settings: &Automation,
    env: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    if model.context_tokens < 8192 {
        return Err(ReviewError::Invalid(
            "Для автоматического ревью нужен контекст модели не меньше 8192 токенов".into(),
        ));
    }
    let missing = |name: &str| env(name).is_none_or(|value| value.trim().is_empty());
    if model.provider == crate::llm::Provider::NeuralDeep && missing("NEURALDEEP_API_KEY") {
        return Err(ReviewError::Invalid(
            "Для NeuralDeep задайте NEURALDEEP_API_KEY до запуска agi".into(),
        ));
    }
    if let Some(name) = &project.embedding.api_key_env
        && missing(name)
    {
        return Err(ReviewError::Invalid(format!(
            "Для эмбеддингов задайте {name} до запуска agi"
        )));
    }
    if settings.publish && missing(&project.token_env) {
        return Err(ReviewError::Invalid(format!(
            "Для автопубликации задайте {} до запуска agi либо выключите автопубликацию",
            project.token_env
        )));
    }
    Ok(())
}

#[derive(Clone)]
struct WatchEvent {
    stage: String,
}
trait Backend {
    async fn analyze(&self, id: Uuid, project: &Project, pull: &PullRequest) -> Result<ReviewRun>;
    async fn publish(&self, project: &Project, run: ReviewRun) -> Result<ReviewRun>;
}
struct NativeBackend {
    store: ReviewStore,
    events: mpsc::UnboundedSender<WatchEvent>,
}
impl Backend for NativeBackend {
    async fn analyze(&self, id: Uuid, project: &Project, pull: &PullRequest) -> Result<ReviewRun> {
        let model = crate::chat::ChatStore::with_directory(self.store.directory.clone())
            .map_err(|e| ReviewError::Invalid(e.to_string()))?
            .llms()
            .active()
            .map_err(|e| ReviewError::Invalid(e.to_string()))?;
        crate::review_pipeline::run_with_id(
            &self.store,
            project.clone(),
            model,
            Some(pull.number),
            Some(&pull.head.sha),
            id,
            |stage| {
                let _ = self.events.send(WatchEvent {
                    stage: format!("{} #{}: {stage}", project.repository, pull.number),
                });
            },
        )
        .await
    }
    async fn publish(&self, project: &Project, mut run: ReviewRun) -> Result<ReviewRun> {
        let _ = self.events.send(WatchEvent {
            stage: format!(
                "{} #{}: публикация",
                project.repository,
                run.pr.unwrap_or_default()
            ),
        });
        crate::review_github::publish(&self.store, project, &mut run).await?;
        Ok(run)
    }
}
fn defer(checkpoint: &mut Checkpoint, message: String) {
    checkpoint.status = "failed".into();
    checkpoint.error = Some(message);
    checkpoint.retry_at = now()
        + (60_u64.saturating_mul(1_u64 << checkpoint.attempts.saturating_sub(1).min(4)))
            .min(config::REVIEW_RETRY_MAX_SECS);
}
async fn process_pull(
    store: &ReviewStore,
    project: &Project,
    pull: &PullRequest,
    backend: &impl Backend,
) -> Result<()> {
    let settings = store.automation(project.id)?;
    if !settings.enabled || pull.draft || pull.state == "closed" {
        return Ok(());
    }
    let mut checkpoint = store.checkpoint(project, pull)?;
    if checkpoint.retry_at > now() {
        return Ok(());
    }
    let cached = match checkpoint.run {
        Some(id) => store.saved_run(id)?.filter(|r| r.status == "completed"),
        None => None,
    };
    if checkpoint.status == "completed"
        && (!settings.publish
            || cached
                .as_ref()
                .is_some_and(|r| r.publication == "published"))
    {
        return Ok(());
    }
    checkpoint.attempts = checkpoint.attempts.saturating_add(1);
    checkpoint.status = "running".into();
    let id = if cached.is_some() {
        checkpoint.run.unwrap_or_else(Uuid::new_v4)
    } else {
        Uuid::new_v4()
    };
    checkpoint.run = Some(id);
    store.save_checkpoint(&checkpoint)?;
    let analysis = match cached {
        Some(run) => Ok(run),
        None => backend.analyze(id, project, pull).await,
    };
    match analysis {
        Ok(mut run) if run.status == "completed" => {
            let latest = store.automation(project.id)?;
            if !latest.enabled || store.project(project.id)?.repository != project.repository {
                return Ok(());
            }
            if latest.publish && run.publication != "published" {
                match backend.publish(project, run).await {
                    Ok(published) => run = published,
                    Err(error) => {
                        defer(&mut checkpoint, error.to_string());
                        store.save_checkpoint(&checkpoint)?;
                        return Ok(());
                    }
                }
            }
            store.save_run(&run)?;
            if run.publication == "failed" {
                defer(
                    &mut checkpoint,
                    run.error
                        .clone()
                        .unwrap_or_else(|| "Ошибка публикации".into()),
                );
            } else {
                checkpoint.status = if run.publication == "stale" {
                    "stale"
                } else {
                    "completed"
                }
                .into();
                checkpoint.error = None;
                checkpoint.retry_at = 0;
            }
        }
        Ok(run) => defer(
            &mut checkpoint,
            run.error.unwrap_or_else(|| "Анализ не завершён".into()),
        ),
        Err(error) => defer(&mut checkpoint, error.to_string()),
    }
    store.save_checkpoint(&checkpoint)?;
    Ok(())
}
async fn scan_project(
    store: &ReviewStore,
    project: &Project,
    github: &Github,
    backend: &impl Backend,
) -> Result<()> {
    let result = async {
        let pulls = github.pulls().await?;
        let mut errors = Vec::new();
        for pull in pulls {
            if pull.draft {
                continue;
            }
            process_pull(store, project, &pull, backend).await?;
            let checkpoint = store.checkpoint(project, &pull)?;
            if checkpoint.status == "failed" {
                errors.push(format!(
                    "PR #{}: {}",
                    pull.number,
                    checkpoint.error.as_deref().unwrap_or("ошибка ревью")
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ReviewError::Invalid(errors.join("; ")))
        }
    }
    .await;
    let error = match &result {
        Ok(()) => None,
        Err(error) => Some(error.to_string()),
    };
    store.polled(project.id, error.as_deref())?;
    result
}
async fn disabled(store: &ReviewStore, project: &Project) {
    loop {
        tokio::time::sleep(Duration::from_secs(config::REVIEW_HEARTBEAT_SECS)).await;
        let enabled = store.automation(project.id).is_ok_and(|a| a.enabled)
            && store
                .project(project.id)
                .is_ok_and(|p| p.repository == project.repository);
        if !enabled {
            return;
        }
    }
}
async fn worker(
    store: &ReviewStore,
    events: mpsc::UnboundedSender<WatchEvent>,
    once: bool,
) -> Result<()> {
    let backend = NativeBackend {
        store: store.clone(),
        events: events.clone(),
    };
    loop {
        for project in store.projects()? {
            let settings = store.automation(project.id)?;
            if !settings.enabled
                || !once
                    && settings.last_poll != 0
                    && now().saturating_sub(settings.last_poll) < settings.interval_secs
            {
                continue;
            }
            let _ = events.send(WatchEvent {
                stage: format!("{}: проверка PR", project.repository),
            });
            let result = async {
                validate_setup(store, &project, &settings)?;
                let github = Github::new(&project)?;
                tokio::select! {
                    result = scan_project(store, &project, &github, &backend) => result,
                    _ = disabled(store, &project) => Ok(()),
                }
            }
            .await;
            if let Err(error) = result {
                store.polled(project.id, Some(&error.to_string()))?;
            }
        }
        let _ = events.send(WatchEvent {
            stage: "Ожидание следующей проверки".into(),
        });
        if once {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub(crate) async fn watch(store: &ReviewStore, once: bool) -> Result<()> {
    let Some(_lock) = acquire(store)? else {
        return Err(ReviewError::Invalid(
            "Фоновый процесс уже работает для этого каталога состояния".into(),
        ));
    };
    store.watch_schema()?;
    let token = Uuid::new_v4().to_string();
    store.connection()?.execute("INSERT INTO review_daemon VALUES(1,?1,?2,?3,0,'Запуск') ON CONFLICT(id) DO UPDATE SET token=excluded.token,pid=excluded.pid,heartbeat=excluded.heartbeat,stop=0,stage=excluded.stage",
        params![token, std::process::id(), now() as i64])?;
    let (sender, mut receiver) = mpsc::unbounded_channel::<WatchEvent>();
    let work = worker(store, sender, once);
    let monitor = async {
        let mut clock = tokio::time::interval(Duration::from_secs(config::REVIEW_HEARTBEAT_SECS));
        let mut stage = "Запуск".to_owned();
        loop {
            tokio::select! {
                event = receiver.recv() => {
                    if let Some(event) = event { if stage != event.stage { eprintln!("{} {}", now(), crate::ui::sanitize_terminal_text(&event.stage)); } stage = event.stage; } else { return Ok(()); }
                }
                _ = clock.tick() => {
                    let stop: bool = store.connection()?.query_row("SELECT stop FROM review_daemon WHERE id=1 AND token=?1", [&token], |row| row.get(0))?;
                    if stop { return Ok::<(), ReviewError>(()); }
                    store.connection()?.execute("UPDATE review_daemon SET heartbeat=?2,stage=?3 WHERE id=1 AND token=?1", params![token, now() as i64, stage])?;
                }
            }
        }
    };
    let result = tokio::select! {
        result = work => result,
        result = monitor => result,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    store.connection()?.execute(
        "UPDATE review_daemon SET heartbeat=?2,stage=?3 WHERE id=1 AND token=?1",
        params![
            token,
            now() as i64,
            if result.is_ok() {
                "Остановлен"
            } else {
                "Ошибка фонового процесса"
            }
        ],
    )?;
    result
}

#[cfg(test)]
#[path = "review_watch_tests.rs"]
mod tests;
