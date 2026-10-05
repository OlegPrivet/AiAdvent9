//! Connections and the application-wide LLM selection, independent of chat history.
use clap::{Parser, Subcommand};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chat::ChatStore;
use crate::settings::Settings;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    NeuralDeep,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LlmProfile {
    pub(crate) id: String,
    pub(crate) provider: Provider,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) context_tokens: u32,
    pub(crate) tools: bool,
    pub(crate) json_schema: bool,
}

impl LlmProfile {
    pub(crate) fn neuraldeep(model: &str) -> Self {
        Self {
            id: format!("neuraldeep:{model}"),
            provider: Provider::NeuralDeep,
            base_url: crate::config::DEFAULT_BASE_URL.into(),
            model: model.into(),
            context_tokens: crate::config::model_context_tokens(model).unwrap_or(0),
            tools: crate::config::model_supports_tools(model),
            json_schema: true,
        }
    }

    pub(crate) fn label(&self) -> String {
        format!(
            "{} · {} · {}",
            self.id,
            Settings::model_label(&self.model),
            self.base_url
        )
    }

    pub(crate) fn validate(&self) -> Result<(), LlmError> {
        if self.id.is_empty()
            || self.id.len() > 120
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(&c))
            || self.model.trim().is_empty()
            || self.model.chars().any(char::is_control)
            || self.context_tokens < 512
        {
            return Err(LlmError::Validation(
                "Нужны корректный ID, модель и контекст не меньше 512 токенов".into(),
            ));
        }
        let url = reqwest::Url::parse(&self.base_url)
            .map_err(|_| LlmError::Validation("Некорректный URL LLM".into()))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(LlmError::Validation(
                "URL должен быть HTTP(S), без ключей, query и fragment".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum LlmError {
    #[error("{0}")]
    Validation(String),
    #[error("ошибка SQLite каталога LLM: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("некорректный профиль LLM: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) struct LlmStore<'a> {
    connection: &'a Connection,
}

impl<'a> LlmStore<'a> {
    pub(crate) fn new(connection: &'a Connection) -> Self {
        Self { connection }
    }

    pub(crate) fn list(&self) -> Result<Vec<LlmProfile>, LlmError> {
        let mut profiles = Settings::model_names()
            .into_iter()
            .map(LlmProfile::neuraldeep)
            .collect::<Vec<_>>();
        let mut statement = self
            .connection
            .prepare("SELECT profile_json FROM llm_profiles ORDER BY id")?;
        for row in statement.query_map([], |row| row.get::<_, String>(0))? {
            let profile: LlmProfile = serde_json::from_str(&row?)?;
            profile.validate()?;
            profiles.push(profile);
        }
        Ok(profiles)
    }

    pub(crate) fn get(&self, id: &str) -> Result<LlmProfile, LlmError> {
        self.list()?
            .into_iter()
            .find(|profile| profile.id == id)
            .ok_or_else(|| LlmError::Validation(format!("Профиль LLM {id} не найден")))
    }

    pub(crate) fn active(&self) -> Result<LlmProfile, LlmError> {
        let id = self
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key='llm_default'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        self.get(&id.unwrap_or_else(|| format!("neuraldeep:{}", crate::config::DEFAULT_MODEL)))
    }

    pub(crate) fn set_default(&self, id: &str) -> Result<LlmProfile, LlmError> {
        let profile = self.get(id)?;
        self.connection.execute("INSERT INTO metadata(key,value) VALUES ('llm_default',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [id])?;
        Ok(profile)
    }

    pub(crate) fn add(&self, mut profile: LlmProfile) -> Result<(), LlmError> {
        profile.base_url = profile.base_url.trim_end_matches('/').into();
        profile.validate()?;
        if profile.id.starts_with("neuraldeep:") || self.list()?.iter().any(|p| p.id == profile.id)
        {
            return Err(LlmError::Validation(
                "Такой ID уже существует или зарезервирован".into(),
            ));
        }
        self.connection.execute(
            "INSERT INTO llm_profiles(id,profile_json) VALUES (?1,?2)",
            params![profile.id, serde_json::to_string(&profile)?],
        )?;
        Ok(())
    }

    pub(crate) fn remove(&self, id: &str) -> Result<(), LlmError> {
        if self.active()?.id == id || id.starts_with("neuraldeep:") {
            return Err(LlmError::Validation(
                "Нельзя удалить активный или встроенный профиль".into(),
            ));
        }
        let mut statement = self
            .connection
            .prepare("SELECT settings_json FROM agents")?;
        for row in statement.query_map([], |row| row.get::<_, String>(0))? {
            let settings: Settings = serde_json::from_str(&row?)?;
            if settings.profile_id() == Some(id) {
                return Err(LlmError::Validation(
                    "Профиль используется агентом; сначала смените его LLM".into(),
                ));
            }
        }
        if self
            .connection
            .execute("DELETE FROM llm_profiles WHERE id=?1", [id])?
            == 0
        {
            return Err(LlmError::Validation(format!("Профиль LLM {id} не найден")));
        }
        Ok(())
    }

    pub(crate) fn resolve_agent(&self, settings: &mut Settings) -> Result<(), LlmError> {
        let profile = match settings.profile_id() {
            Some(id) => self.get(id)?,
            None => self.get(&format!("neuraldeep:{}", settings.model()))?,
        };
        settings.apply_profile(profile);
        Ok(())
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum LlmCommand {
    /// Показать подключения и активную модель.
    List,
    /// Добавить локальный OpenAI-совместимый endpoint (URL до /v1).
    Add {
        id: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        model: String,
        #[arg(long)]
        context_tokens: u32,
        #[arg(long)]
        tools: bool,
        #[arg(long)]
        json_schema: bool,
    },
    /// Выбрать LLM для всех чатов.
    Default {
        id: String,
    },
    Remove {
        id: String,
    },
    /// Однократный запрос без интерактивного режима.
    Ask {
        question: String,
        #[arg(long)]
        profile: Option<String>,
    },
}

#[derive(Parser)]
struct SlashArgs {
    #[command(subcommand)]
    command: LlmCommand,
}

pub(crate) fn command(store: &LlmStore<'_>, command: LlmCommand) -> Result<String, LlmError> {
    match command {
        LlmCommand::List => {
            let active = store.active()?.id;
            Ok(store
                .list()?
                .iter()
                .map(|p| format!("{} {}", if p.id == active { "*" } else { " " }, p.label()))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        LlmCommand::Default { id } => Ok(format!("Общая LLM: {}", store.set_default(&id)?.label())),
        LlmCommand::Add {
            id,
            url,
            model,
            context_tokens,
            tools,
            json_schema,
        } => {
            store.add(LlmProfile {
                id: id.clone(),
                provider: Provider::Local,
                base_url: url,
                model,
                context_tokens,
                tools,
                json_schema,
            })?;
            Ok(format!("Профиль {id} добавлен"))
        }
        LlmCommand::Remove { id } => {
            store.remove(&id)?;
            Ok(format!("Профиль {id} удалён"))
        }
        LlmCommand::Ask { .. } => Err(LlmError::Validation(
            "Однократный запрос: agi llm ask <вопрос> [--profile ID]".into(),
        )),
    }
}

pub(crate) fn slash(store: &ChatStore, argument: Option<&str>) -> Result<String, LlmError> {
    // Arguments are parsed by Clap; no shell is invoked.
    let args = argument
        .unwrap_or("list")
        .split_whitespace()
        .map(str::to_owned);
    let parsed = SlashArgs::try_parse_from(std::iter::once("llm".to_owned()).chain(args))
        .map_err(|e| LlmError::Validation(e.to_string()))?;
    command(&store.llms(), parsed.command)
}

pub(crate) fn sync_chat(store: &ChatStore, chat: &mut crate::chat::Chat) -> Result<(), LlmError> {
    chat.settings_mut().apply_profile(store.llms().active()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE llm_profiles(id TEXT PRIMARY KEY,profile_json TEXT NOT NULL); CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE agents(settings_json TEXT);").unwrap();
        connection
    }
    fn local() -> LlmProfile {
        LlmProfile {
            id: "local".into(),
            provider: Provider::Local,
            base_url: "http://localhost:11434/v1".into(),
            model: "test".into(),
            context_tokens: 16384,
            tools: false,
            json_schema: false,
        }
    }
    #[test]
    fn persists_default_and_protects_active_profile() {
        let connection = connection();
        let store = LlmStore::new(&connection);
        store.add(local()).unwrap();
        store.set_default("local").unwrap();
        assert_eq!(LlmStore::new(&connection).active().unwrap().id, "local");
        assert!(store.remove("local").is_err());
        store
            .set_default(&LlmProfile::neuraldeep(crate::config::DEFAULT_MODEL).id)
            .unwrap();
        store.remove("local").unwrap();
    }
    #[test]
    fn rejects_invalid_profiles_and_unknown_default() {
        let connection = connection();
        let store = LlmStore::new(&connection);
        let mut profile = local();
        profile.base_url = "http://secret@localhost/v1".into();
        assert!(store.add(profile).is_err());
        assert!(store.set_default("missing").is_err());
        assert_eq!(store.active().unwrap().model, crate::config::DEFAULT_MODEL);
    }
}
