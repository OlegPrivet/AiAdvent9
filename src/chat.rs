use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::context::{ContextStrategyKind, Facts, MAX_FACTS, validate_fact, validate_facts};
use crate::memory::{
    MAX_WORKING_ENTRIES, MemoryRef, MemorySelection, MemoryStore, WorkingMemory,
    validate_working_entry, validate_working_memory,
};
use crate::metrics::ResponseMetrics;
use crate::pricing::PriceCatalog;
use crate::settings::Settings;

const LEGACY_CHAT_SCHEMA_VERSION: u32 = 1;
const DATABASE_SCHEMA_VERSION: i64 = 9;
const DATABASE_FILE_NAME: &str = "chats.sqlite3";
const LEGACY_DIRECTORY_NAME: &str = "chats";
const LEGACY_IMPORT_KEY: &str = "legacy_json_imported";
const MAX_TITLE_CHARS: usize = 60;
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    pub(crate) const fn as_api_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    fn from_database(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
    pub(crate) role: MessageRole,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) metrics: Option<ResponseMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Chat {
    schema_version: u32,
    id: Uuid,
    title: String,
    created_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default)]
    settings: Settings,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    summary: Option<crate::summary::ConversationSummary>,
    #[serde(default)]
    facts: Facts,
    #[serde(default)]
    working_memory: WorkingMemory,
    #[serde(default)]
    memory_selection: MemorySelection,
    #[serde(default)]
    task: Option<crate::task::TaskState>,
    #[serde(default)]
    branch_group_id: Option<Uuid>,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    parent_checkpoint_id: Option<Uuid>,
    #[serde(skip)]
    persisted: bool,
    #[serde(skip)]
    dirty: bool,
}

impl Chat {
    pub(crate) fn new() -> Self {
        let now = now_millis();
        let id = Uuid::new_v4();
        Self {
            schema_version: LEGACY_CHAT_SCHEMA_VERSION,
            id,
            title: "Новый чат".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
            settings: Settings::default(),
            messages: Vec::new(),
            summary: None,
            facts: Facts::new(),
            working_memory: WorkingMemory::new(),
            memory_selection: MemorySelection::default(),
            task: None,
            branch_group_id: Some(id),
            branch_name: Some("main".to_owned()),
            parent_checkpoint_id: None,
            persisted: false,
            dirty: false,
        }
    }

    pub(crate) fn id(&self) -> Uuid {
        self.id
    }

    pub(crate) fn task(&self) -> Option<&crate::task::TaskState> {
        self.task.as_ref()
    }

    pub(crate) fn set_task(&mut self, task: crate::task::TaskState) {
        if !self.has_completed_turn() {
            self.title = title_from_question(&task.goal);
        }
        self.task = Some(task);
        self.mark_changed();
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }

    pub(crate) fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    pub(crate) fn summary(&self) -> Option<&crate::summary::ConversationSummary> {
        self.summary.as_ref()
    }

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(crate) fn facts(&self) -> &Facts {
        &self.facts
    }

    pub(crate) fn working_memory(&self) -> &WorkingMemory {
        &self.working_memory
    }

    pub(crate) fn memory_selection(&self) -> &MemorySelection {
        &self.memory_selection
    }

    pub(crate) fn set_working(&mut self, key: String, value: String) -> Result<(), String> {
        validate_working_entry(&key, &value)?;
        if !self.working_memory.contains_key(&key)
            && self.working_memory.len() >= MAX_WORKING_ENTRIES
        {
            return Err(format!(
                "Допускается не более {MAX_WORKING_ENTRIES} записей рабочей памяти."
            ));
        }
        self.working_memory.insert(key, value.trim().to_owned());
        self.mark_changed();
        Ok(())
    }

    pub(crate) fn delete_working(&mut self, key: &str) -> bool {
        let changed = self.working_memory.remove(key).is_some();
        if changed {
            self.mark_changed();
        }
        changed
    }

    pub(crate) fn clear_working(&mut self) -> bool {
        let changed = !self.working_memory.is_empty();
        if changed {
            self.working_memory.clear();
            self.mark_changed();
        }
        changed
    }

    pub(crate) fn set_profile_enabled(&mut self, enabled: bool) -> bool {
        let changed = self.memory_selection.profile != enabled;
        if changed {
            self.memory_selection.profile = enabled;
            self.mark_changed();
        }
        changed
    }

    pub(crate) fn select_profile(&mut self, name: String) -> bool {
        let changed = !self.memory_selection.profile || self.memory_selection.profile_name != name;
        if changed {
            self.memory_selection.profile = true;
            self.memory_selection.profile_name = name;
            self.mark_changed();
        }
        changed
    }

    pub(crate) fn use_memory(&mut self, reference: MemoryRef) -> bool {
        let changed = self.memory_selection.entries.insert(reference);
        if changed {
            self.mark_changed();
        }
        changed
    }

    pub(crate) fn unuse_memory(&mut self, reference: &MemoryRef) -> bool {
        let changed = self.memory_selection.entries.remove(reference);
        if changed {
            self.mark_changed();
        }
        changed
    }

    pub(crate) fn branch_group_id(&self) -> Uuid {
        self.branch_group_id.unwrap_or(self.id)
    }

    pub(crate) fn branch_name(&self) -> &str {
        self.branch_name.as_deref().unwrap_or("main")
    }

    pub(crate) fn set_fact(&mut self, key: String, value: String) -> Result<(), String> {
        validate_fact(&key, &value)?;
        if !self.facts.contains_key(&key) && self.facts.len() >= MAX_FACTS {
            return Err(format!("Допускается не более {MAX_FACTS} фактов."));
        }
        self.facts.insert(key, value);
        self.mark_changed();
        Ok(())
    }

    pub(crate) fn delete_fact(&mut self, key: &str) -> bool {
        let removed = self.facts.remove(key).is_some();
        if removed {
            self.mark_changed();
        }
        removed
    }

    pub(crate) fn has_completed_turn(&self) -> bool {
        self.summary.is_some() || !self.messages.is_empty()
    }

    pub(crate) fn has_persistable_state(&self) -> bool {
        self.has_completed_turn()
            || self.task.is_some()
            || !self.working_memory.is_empty()
            || self.memory_selection != MemorySelection::default()
    }

    pub(crate) fn is_persisted(&self) -> bool {
        self.persisted
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[cfg(test)]
    pub(crate) fn record_exchange(&mut self, question: String, answer: String) {
        self.record_exchange_with_metrics(question, answer, None);
    }

    #[cfg(test)]
    pub(crate) fn record_exchange_with_metrics(
        &mut self,
        question: String,
        answer: String,
        metrics: Option<ResponseMetrics>,
    ) {
        self.record_exchange_with_context(question, answer, metrics, None);
    }

    pub(crate) fn record_exchange_with_context(
        &mut self,
        question: String,
        answer: String,
        mut metrics: Option<ResponseMetrics>,
        facts: Option<Facts>,
    ) {
        let had_previous_usage = self.has_completed_turn();
        let previous_usage = self.cumulative_api_usage();
        if let Some(metrics) = metrics.as_mut() {
            let new_usage = metrics.usage.map(|usage| {
                usage.saturating_sub(metrics.already_counted_usage.unwrap_or_default())
            });
            metrics.cumulative_usage = if had_previous_usage {
                previous_usage
                    .zip(new_usage)
                    .map(|(previous, current)| previous.saturating_add(current))
            } else {
                new_usage
            };
            metrics.already_counted_usage = None;
        }
        if self.messages.is_empty() && self.summary.is_none() && self.task.is_none() {
            self.title = title_from_question(&question);
        }
        self.messages.push(ChatMessage {
            role: MessageRole::User,
            content: question,
            metrics: None,
        });
        self.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: answer,
            metrics,
        });
        if let Some(facts) = facts {
            debug_assert!(validate_facts(&facts).is_ok());
            self.facts = facts;
        }
        self.prune_context_messages();
        self.mark_changed();
    }

    fn prune_context_messages(&mut self) {
        let strategy = self.settings.context_strategy();
        if strategy.kind == ContextStrategyKind::Branching
            || self.messages.len() <= strategy.max_messages
        {
            return;
        }
        let remove = self.messages.len() - strategy.max_messages;
        self.messages.drain(..remove);
    }

    pub(crate) fn last_response_metrics(&self) -> Option<&ResponseMetrics> {
        self.messages
            .iter()
            .rev()
            .find_map(|message| message.metrics.as_ref())
            .or_else(|| self.summary.as_ref().map(|summary| &summary.metrics))
    }

    pub(crate) fn cumulative_api_usage(&self) -> Option<crate::metrics::TokenUsage> {
        if let Some(usage) = self
            .last_response_metrics()
            .and_then(|metrics| metrics.cumulative_usage)
        {
            return Some(usage);
        }

        let total = self
            .summary
            .iter()
            .map(|summary| &summary.metrics)
            .try_fold(crate::metrics::TokenUsage::default(), |total, metrics| {
                Some(total.saturating_add(metrics.usage?))
            })?;
        self.messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .try_fold(total, |total, message| {
                Some(total.saturating_add(message.metrics.as_ref()?.usage?))
            })
    }

    pub(crate) fn refresh_last_response_cost(&mut self, prices: &PriceCatalog) -> bool {
        let last = self
            .messages
            .iter_mut()
            .rev()
            .find_map(|message| message.metrics.as_mut());
        let summary = self.summary.as_mut().map(|summary| &mut summary.metrics);
        let mut changed = false;
        for metrics in last.into_iter().chain(summary) {
            if metrics.estimated_cost_microrubles.is_some() {
                continue;
            }
            let previous = (metrics.estimated_cost_microrubles, metrics.premium);
            metrics.refresh_cost(prices);
            changed |= previous != (metrics.estimated_cost_microrubles, metrics.premium);
        }
        if changed {
            self.mark_changed();
        }
        changed
    }

    pub(crate) fn mark_changed(&mut self) {
        self.updated_at_ms = now_millis();
        self.dirty = true;
    }

    fn mark_saved(&mut self) {
        self.persisted = true;
        self.dirty = false;
    }

    fn mark_loaded(&mut self) {
        self.persisted = true;
        self.dirty = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChatSummary {
    pub(crate) id: Uuid,
    pub(crate) title: String,
}

impl fmt::Display for ChatSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} [{}]", self.title, self.id)
    }
}

#[derive(Debug)]
pub(crate) struct ChatList {
    pub(crate) chats: Vec<ChatSummary>,
    pub(crate) skipped_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchInfo {
    pub(crate) id: Uuid,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckpointInfo {
    pub(crate) id: Uuid,
    pub(crate) name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointSnapshot {
    title: String,
    settings: Settings,
    messages: Vec<ChatMessage>,
    summary: Option<crate::summary::ConversationSummary>,
    facts: Facts,
    #[serde(default)]
    working_memory: WorkingMemory,
    #[serde(default)]
    memory_selection: MemorySelection,
    #[serde(default)]
    task: Option<crate::task::TaskState>,
}

#[derive(Debug)]
pub(crate) struct ChatStore {
    connection: Connection,
    database_path: PathBuf,
}

#[derive(Debug, Error)]
pub(crate) enum ChatStoreError {
    #[error("не удалось определить каталог для истории чатов")]
    MissingStateDirectory,
    #[error("не удалось {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("ошибка SQLite при операции «{action}» ({path}): {source}")]
    Database {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("SQLite-база использует неподдерживаемую версию схемы {0}")]
    UnsupportedSchema(i64),
    #[error("чат {0} не найден")]
    NotFound(Uuid),
    #[error("не удалось {action} настройки чата {id}: {source}")]
    SettingsJson {
        action: &'static str,
        id: Uuid,
        #[source]
        source: serde_json::Error,
    },
    #[error("не удалось {action} метрики сообщения {sequence} чата {id}: {source}")]
    MessageMetricsJson {
        action: &'static str,
        id: Uuid,
        sequence: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("чат {0} содержит некорректную последовательность сообщений")]
    InvalidConversation(Uuid),
    #[error("чат {0} содержит некорректную временную метку")]
    InvalidTimestamp(Uuid),
    #[error("{0}")]
    InvalidName(String),
    #[error("checkpoint «{0}» не найден в текущей группе веток")]
    CheckpointNotFound(String),
    #[error("ветка «{0}» не найдена в текущей группе веток")]
    BranchNotFound(String),
}

impl ChatStore {
    pub(crate) fn agents(&self) -> crate::agent_catalog::AgentStore<'_> {
        crate::agent_catalog::AgentStore::new(&self.connection)
    }

    pub(crate) fn memory(&self) -> MemoryStore {
        MemoryStore::new(
            self.database_path
                .parent()
                .expect("путь базы всегда содержит каталог"),
        )
    }

    pub(crate) fn invariants(&self) -> crate::invariants::InvariantStore<'_> {
        crate::invariants::InvariantStore::new(&self.connection)
    }

    pub(crate) fn open() -> Result<Self, ChatStoreError> {
        let directory = state_directory().ok_or(ChatStoreError::MissingStateDirectory)?;
        Self::with_directory(directory)
    }

    fn with_directory(directory: PathBuf) -> Result<Self, ChatStoreError> {
        fs::create_dir_all(&directory).map_err(|source| ChatStoreError::Io {
            action: "создать каталог",
            path: directory.clone(),
            source,
        })?;
        set_private_directory_permissions(&directory)?;

        let database_path = directory.join(DATABASE_FILE_NAME);
        let mut connection = Connection::open(&database_path)
            .map_err(|source| database_error("открыть", &database_path, source))?;
        set_private_file_permissions(&database_path)?;
        connection.set_transaction_behavior(TransactionBehavior::Immediate);
        connection
            .busy_timeout(DATABASE_BUSY_TIMEOUT)
            .map_err(|source| database_error("настроить", &database_path, source))?;
        initialize_database(&connection, &database_path)?;

        let store = Self {
            connection,
            database_path,
        };
        store.import_legacy_json(&directory.join(LEGACY_DIRECTORY_NAME))?;
        Ok(store)
    }

    pub(crate) fn load(&self, id: Uuid) -> Result<Chat, ChatStoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT title, created_at_ms, updated_at_ms, settings_json, summary_json,
                        facts_json, branch_group_id, branch_name, parent_checkpoint_id, task_state_json
                 FROM chats WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .optional()
            .map_err(self.database_error("прочитать"))?;
        let Some((
            title,
            created_at_ms,
            updated_at_ms,
            settings_json,
            summary_json,
            facts_json,
            branch_group_id,
            branch_name,
            parent_checkpoint_id,
            task_state_json,
        )) = row
        else {
            return Err(ChatStoreError::NotFound(id));
        };
        let settings = serde_json::from_str(&settings_json).map_err(|source| {
            ChatStoreError::SettingsJson {
                action: "прочитать",
                id,
                source,
            }
        })?;
        let facts: Facts =
            serde_json::from_str(&facts_json).map_err(|source| ChatStoreError::SettingsJson {
                action: "прочитать facts",
                id,
                source,
            })?;
        validate_facts(&facts).map_err(|_| ChatStoreError::InvalidConversation(id))?;
        let task: Option<crate::task::TaskState> = task_state_json
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|source| ChatStoreError::SettingsJson {
                action: "прочитать состояние задачи",
                id,
                source,
            })?;
        if let Some(task) = &task {
            task.validate()
                .map_err(|_| ChatStoreError::InvalidConversation(id))?;
        }
        let mut working_memory = WorkingMemory::new();
        {
            let mut statement = self
                .connection
                .prepare("SELECT key, value FROM working_memory WHERE chat_id = ?1 ORDER BY key")
                .map_err(|source| {
                    self.database_error_with_source("прочитать рабочую память", source)
                })?;
            let rows = statement
                .query_map(params![id.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|source| {
                    self.database_error_with_source("прочитать рабочую память", source)
                })?;
            for row in rows {
                let (key, value) = row.map_err(|source| {
                    self.database_error_with_source("прочитать рабочую память", source)
                })?;
                working_memory.insert(key, value);
            }
        }
        validate_working_memory(&working_memory)
            .map_err(|_| ChatStoreError::InvalidConversation(id))?;
        let profile_settings = self
            .connection
            .query_row(
                "SELECT profile_enabled, profile_name FROM memory_settings WHERE chat_id = ?1",
                params![id.to_string()],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(self.database_error("прочитать настройки памяти"))?
            .unwrap_or_else(|| (true, crate::memory::DEFAULT_PROFILE_NAME.into()));
        crate::memory::validate_name(&profile_settings.1)
            .map_err(|_| ChatStoreError::InvalidConversation(id))?;
        let mut memory_selection = MemorySelection {
            profile: profile_settings.0,
            profile_name: profile_settings.1,
            ..MemorySelection::default()
        };
        {
            let mut statement = self
                .connection
                .prepare("SELECT kind, name FROM memory_selections WHERE chat_id = ?1 ORDER BY kind, name")
                .map_err(|source| self.database_error_with_source("прочитать подключения памяти", source))?;
            let rows = statement
                .query_map(params![id.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|source| {
                    self.database_error_with_source("прочитать подключения памяти", source)
                })?;
            for row in rows {
                let (kind, name) = row.map_err(|source| {
                    self.database_error_with_source("прочитать подключения памяти", source)
                })?;
                let kind = match kind.as_str() {
                    "decision" => crate::memory::LongTermKind::Decision,
                    "knowledge" => crate::memory::LongTermKind::Knowledge,
                    _ => return Err(ChatStoreError::InvalidConversation(id)),
                };
                memory_selection.entries.insert(MemoryRef { kind, name });
            }
        }
        let branch_group_id = branch_group_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| ChatStoreError::InvalidConversation(id))?
            .or(Some(id));
        let parent_checkpoint_id = parent_checkpoint_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| ChatStoreError::InvalidConversation(id))?;

        let mut summary: Option<crate::summary::ConversationSummary> = summary_json
            .map(|text| serde_json::from_str(&text))
            .transpose()
            .map_err(|source| ChatStoreError::SettingsJson {
                action: "прочитать резюме",
                id,
                source,
            })?;
        // Older saved summaries predate the explicit operation marker.
        if let Some(summary) = summary.as_mut() {
            summary.metrics.is_summary = true;
        }
        if summary
            .as_ref()
            .is_some_and(|summary| summary.content.trim().is_empty())
        {
            return Err(ChatStoreError::InvalidConversation(id));
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, role, content, response_metrics_json
                 FROM messages WHERE chat_id = ?1 ORDER BY sequence",
            )
            .map_err(|source| self.database_error_with_source("подготовить чтение", source))?;
        let rows = statement
            .query_map(params![id.to_string()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|source| self.database_error_with_source("прочитать сообщения", source))?;
        let mut messages = Vec::new();
        for (expected_sequence, row) in rows.enumerate() {
            let (sequence, role, content, metrics_json) = row
                .map_err(|source| self.database_error_with_source("прочитать сообщение", source))?;
            let Some(role) = MessageRole::from_database(&role) else {
                return Err(ChatStoreError::InvalidConversation(id));
            };
            if sequence != expected_sequence as i64 {
                return Err(ChatStoreError::InvalidConversation(id));
            }
            let metrics = metrics_json
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(|source| ChatStoreError::MessageMetricsJson {
                    action: "прочитать",
                    id,
                    sequence: expected_sequence,
                    source,
                })?;
            if role == MessageRole::User && metrics.is_some() {
                return Err(ChatStoreError::InvalidConversation(id));
            }
            messages.push(ChatMessage {
                role,
                content,
                metrics,
            });
        }
        if !(valid_messages(&messages) || messages.is_empty()) {
            return Err(ChatStoreError::InvalidConversation(id));
        }

        let mut chat = Chat {
            schema_version: LEGACY_CHAT_SCHEMA_VERSION,
            id,
            title,
            created_at_ms: u64::try_from(created_at_ms)
                .map_err(|_| ChatStoreError::InvalidTimestamp(id))?,
            updated_at_ms: u64::try_from(updated_at_ms)
                .map_err(|_| ChatStoreError::InvalidTimestamp(id))?,
            settings,
            messages,
            summary,
            facts,
            working_memory,
            memory_selection,
            task,
            branch_group_id,
            branch_name: branch_name.or_else(|| Some("main".to_owned())),
            parent_checkpoint_id,
            persisted: false,
            dirty: false,
        };
        chat.mark_loaded();
        if let Some(mut task) = chat.task.clone()
            && task.stage != crate::task::TaskStage::Done
            && !task.paused
        {
            task.pause("Чат восстановлен. Продолжение: /task resume.");
            chat.set_task(task);
        }
        Ok(chat)
    }

    pub(crate) fn list(&self) -> Result<ChatList, ChatStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, title, updated_at_ms
                 FROM chats ORDER BY updated_at_ms DESC, id ASC",
            )
            .map_err(|source| self.database_error_with_source("подготовить список", source))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|source| self.database_error_with_source("прочитать список", source))?;

        let mut chats = Vec::new();
        let mut skipped_entries = 0;
        for row in rows {
            let Ok((id, title, updated_at_ms)) = row else {
                skipped_entries += 1;
                continue;
            };
            let Ok(id) = Uuid::parse_str(&id) else {
                skipped_entries += 1;
                continue;
            };
            if updated_at_ms < 0 {
                skipped_entries += 1;
                continue;
            }
            chats.push(ChatSummary { id, title });
        }

        Ok(ChatList {
            chats,
            skipped_entries,
        })
    }

    pub(crate) fn create_checkpoint(
        &self,
        chat: &Chat,
        name: &str,
    ) -> Result<Uuid, ChatStoreError> {
        validate_context_name(name, true).map_err(ChatStoreError::InvalidName)?;
        if chat.settings.context_strategy().kind != ContextStrategyKind::Branching {
            return Err(ChatStoreError::InvalidName(
                "Checkpoint доступен только для стратегии Branching.".into(),
            ));
        }
        if !chat.has_completed_turn() {
            return Err(ChatStoreError::InvalidName(
                "Checkpoint можно создать только после завершённого ответа.".into(),
            ));
        }
        let snapshot = CheckpointSnapshot {
            title: chat.title.clone(),
            settings: chat.settings.clone(),
            messages: chat.messages.clone(),
            summary: chat.summary.clone(),
            facts: chat.facts.clone(),
            working_memory: chat.working_memory.clone(),
            memory_selection: chat.memory_selection.clone(),
            task: chat.task.clone(),
        };
        let snapshot_json =
            serde_json::to_string(&snapshot).map_err(|source| ChatStoreError::SettingsJson {
                action: "сохранить checkpoint",
                id: chat.id,
                source,
            })?;
        let id = Uuid::new_v4();
        self.connection
            .execute(
                "INSERT INTO checkpoints(id, branch_group_id, name, source_chat_id,
                                          created_at_ms, snapshot_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id.to_string(),
                    chat.branch_group_id().to_string(),
                    name,
                    chat.id.to_string(),
                    timestamp_for_database(id, now_millis())?,
                    snapshot_json
                ],
            )
            .map_err(|source| self.database_error_with_source("сохранить checkpoint", source))?;
        Ok(id)
    }

    pub(crate) fn create_branch(
        &self,
        group_id: Uuid,
        checkpoint_name: &str,
        branch_name: &str,
    ) -> Result<Chat, ChatStoreError> {
        validate_context_name(checkpoint_name, true).map_err(ChatStoreError::InvalidName)?;
        validate_context_name(branch_name, false).map_err(ChatStoreError::InvalidName)?;
        let row = self
            .connection
            .query_row(
                "SELECT id, snapshot_json FROM checkpoints
                 WHERE branch_group_id = ?1 AND name = ?2 COLLATE NOCASE",
                params![group_id.to_string(), checkpoint_name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(self.database_error("прочитать checkpoint"))?;
        let Some((checkpoint_id, snapshot_json)) = row else {
            return Err(ChatStoreError::CheckpointNotFound(checkpoint_name.into()));
        };
        let snapshot: CheckpointSnapshot =
            serde_json::from_str(&snapshot_json).map_err(|source| {
                ChatStoreError::SettingsJson {
                    action: "прочитать checkpoint",
                    id: group_id,
                    source,
                }
            })?;
        let id = Uuid::new_v4();
        let now = now_millis();
        let mut chat = Chat {
            schema_version: LEGACY_CHAT_SCHEMA_VERSION,
            id,
            title: format!("{} · {}", snapshot.title, branch_name),
            created_at_ms: now,
            updated_at_ms: now,
            settings: snapshot.settings,
            messages: snapshot.messages,
            summary: snapshot.summary,
            facts: snapshot.facts,
            working_memory: snapshot.working_memory,
            memory_selection: snapshot.memory_selection,
            task: snapshot.task,
            branch_group_id: Some(group_id),
            branch_name: Some(branch_name.to_owned()),
            parent_checkpoint_id: Some(
                Uuid::parse_str(&checkpoint_id)
                    .map_err(|_| ChatStoreError::InvalidConversation(group_id))?,
            ),
            persisted: false,
            dirty: true,
        };
        self.save(&mut chat)?;
        Ok(chat)
    }

    pub(crate) fn branches(&self, group_id: Uuid) -> Result<Vec<BranchInfo>, ChatStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, branch_name FROM chats
                 WHERE branch_group_id = ?1 ORDER BY created_at_ms, branch_name COLLATE NOCASE",
            )
            .map_err(|source| {
                self.database_error_with_source("подготовить список веток", source)
            })?;
        let rows = statement
            .query_map(params![group_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| self.database_error_with_source("прочитать ветки", source))?;
        let mut result = Vec::new();
        for row in rows {
            let (id, name) =
                row.map_err(|source| self.database_error_with_source("прочитать ветку", source))?;
            let id =
                Uuid::parse_str(&id).map_err(|_| ChatStoreError::InvalidConversation(group_id))?;
            result.push(BranchInfo { id, name });
        }
        Ok(result)
    }

    pub(crate) fn checkpoints(
        &self,
        group_id: Uuid,
    ) -> Result<Vec<CheckpointInfo>, ChatStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, name FROM checkpoints
                 WHERE branch_group_id = ?1 ORDER BY created_at_ms, name COLLATE NOCASE",
            )
            .map_err(|source| {
                self.database_error_with_source("подготовить список checkpoints", source)
            })?;
        let rows = statement
            .query_map(params![group_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| self.database_error_with_source("прочитать checkpoints", source))?;
        let mut result = Vec::new();
        for row in rows {
            let (id, name) = row.map_err(|source| {
                self.database_error_with_source("прочитать checkpoint", source)
            })?;
            let id =
                Uuid::parse_str(&id).map_err(|_| ChatStoreError::InvalidConversation(group_id))?;
            result.push(CheckpointInfo { id, name });
        }
        Ok(result)
    }

    pub(crate) fn load_branch(
        &self,
        group_id: Uuid,
        name_or_id: &str,
    ) -> Result<Chat, ChatStoreError> {
        if let Ok(id) = Uuid::parse_str(name_or_id) {
            let chat = self.load(id)?;
            return (chat.branch_group_id() == group_id)
                .then_some(chat)
                .ok_or_else(|| ChatStoreError::BranchNotFound(name_or_id.into()));
        }
        let id = self
            .connection
            .query_row(
                "SELECT id FROM chats
                 WHERE branch_group_id = ?1 AND branch_name = ?2 COLLATE NOCASE",
                params![group_id.to_string(), name_or_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(self.database_error("найти ветку"))?
            .ok_or_else(|| ChatStoreError::BranchNotFound(name_or_id.into()))?;
        let id = Uuid::parse_str(&id).map_err(|_| ChatStoreError::InvalidConversation(group_id))?;
        self.load(id)
    }

    pub(crate) fn replace_with_summary(
        &self,
        chat: &mut Chat,
        mut summary: crate::summary::ConversationSummary,
    ) -> Result<(), ChatStoreError> {
        if summary.content.trim().is_empty() {
            return Err(ChatStoreError::InvalidConversation(chat.id));
        }
        let previous_usage = chat.cumulative_api_usage();
        summary.metrics.cumulative_usage = previous_usage
            .zip(summary.metrics.usage)
            .map(|(previous, current)| previous.saturating_add(current));
        summary.metrics.is_summary = true;
        let mut replacement = chat.clone();
        replacement.messages.clear();
        replacement.summary = Some(summary);
        replacement.mark_changed();
        self.save(&mut replacement)?;
        *chat = replacement;
        Ok(())
    }

    pub(crate) fn save(&self, chat: &mut Chat) -> Result<bool, ChatStoreError> {
        if let Some(task) = &chat.task {
            task.validate()
                .map_err(|_| ChatStoreError::InvalidConversation(chat.id))?;
        }
        if !chat.has_persistable_state() && !chat.is_persisted() {
            return Ok(false);
        }
        if chat.is_persisted() && !chat.is_dirty() {
            return Ok(true);
        }
        if !(valid_messages(&chat.messages) || chat.messages.is_empty()) {
            return Err(ChatStoreError::InvalidConversation(chat.id));
        }

        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|source| self.database_error_with_source("начать транзакцию", source))?;
        write_chat(&transaction, chat, WriteMode::Replace, &self.database_path)?;
        transaction.commit().map_err(|source| {
            self.database_error_with_source("зафиксировать транзакцию", source)
        })?;
        chat.mark_saved();
        Ok(true)
    }

    fn import_legacy_json(&self, directory: &Path) -> Result<(), ChatStoreError> {
        let already_imported = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM metadata WHERE key = ?1)",
                params![LEGACY_IMPORT_KEY],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|source| self.database_error_with_source("проверить миграцию", source))?;
        if already_imported {
            return Ok(());
        }

        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|source| self.database_error_with_source("начать импорт", source))?;
        if directory.exists() {
            let entries = fs::read_dir(directory).map_err(|source| ChatStoreError::Io {
                action: "прочитать старую историю",
                path: directory.to_owned(),
                source,
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(chat) = read_legacy_chat(&path) {
                    write_chat(
                        &transaction,
                        &chat,
                        WriteMode::IgnoreExisting,
                        &self.database_path,
                    )?;
                }
            }
        }
        transaction
            .execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                params![LEGACY_IMPORT_KEY, "1"],
            )
            .map_err(|source| self.database_error_with_source("завершить импорт", source))?;
        transaction
            .commit()
            .map_err(|source| self.database_error_with_source("зафиксировать импорт", source))?;
        Ok(())
    }

    fn database_error(
        &self,
        action: &'static str,
    ) -> impl FnOnce(rusqlite::Error) -> ChatStoreError {
        let path = self.database_path.clone();
        move |source| database_error(action, &path, source)
    }

    fn database_error_with_source(
        &self,
        action: &'static str,
        source: rusqlite::Error,
    ) -> ChatStoreError {
        database_error(action, &self.database_path, source)
    }

    #[cfg(test)]
    pub(crate) fn for_tests(directory: PathBuf) -> Result<Self, ChatStoreError> {
        Self::with_directory(directory)
    }
}

#[derive(Clone, Copy)]
enum WriteMode {
    Replace,
    IgnoreExisting,
}

fn write_chat(
    transaction: &Transaction<'_>,
    chat: &Chat,
    mode: WriteMode,
    database_path: &Path,
) -> Result<bool, ChatStoreError> {
    let settings_json =
        serde_json::to_string(&chat.settings).map_err(|source| ChatStoreError::SettingsJson {
            action: "сохранить",
            id: chat.id,
            source,
        })?;
    let summary_json = chat
        .summary
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|source| ChatStoreError::SettingsJson {
            action: "сохранить резюме",
            id: chat.id,
            source,
        })?;
    let facts_json =
        serde_json::to_string(&chat.facts).map_err(|source| ChatStoreError::SettingsJson {
            action: "сохранить facts",
            id: chat.id,
            source,
        })?;
    let created_at_ms = timestamp_for_database(chat.id, chat.created_at_ms)?;
    let task_state_json = chat
        .task
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|source| ChatStoreError::SettingsJson {
            action: "сохранить состояние задачи",
            id: chat.id,
            source,
        })?;
    let updated_at_ms = timestamp_for_database(chat.id, chat.updated_at_ms)?;
    let id = chat.id.to_string();

    let changed = match mode {
        WriteMode::Replace => transaction.execute(
            "INSERT INTO chats(id, title, created_at_ms, updated_at_ms, settings_json, summary_json,
                               facts_json, branch_group_id, branch_name, parent_checkpoint_id, task_state_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title,
                 created_at_ms = excluded.created_at_ms,
                 updated_at_ms = excluded.updated_at_ms,
                 settings_json = excluded.settings_json, summary_json = excluded.summary_json,
                 facts_json = excluded.facts_json, branch_group_id = excluded.branch_group_id,
                 branch_name = excluded.branch_name,
                 parent_checkpoint_id = excluded.parent_checkpoint_id,
                 task_state_json = excluded.task_state_json",
            params![
                id,
                chat.title,
                created_at_ms,
                updated_at_ms,
                settings_json,
                summary_json,
                facts_json,
                chat.branch_group_id().to_string(),
                chat.branch_name(),
                chat.parent_checkpoint_id.map(|value| value.to_string()),
                task_state_json
            ],
        ),
        WriteMode::IgnoreExisting => transaction.execute(
            "INSERT OR IGNORE INTO chats(id, title, created_at_ms, updated_at_ms, settings_json,
                                         summary_json, facts_json, branch_group_id, branch_name,
                                         parent_checkpoint_id, task_state_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                chat.title,
                created_at_ms,
                updated_at_ms,
                settings_json,
                summary_json,
                facts_json,
                chat.branch_group_id().to_string(),
                chat.branch_name(),
                chat.parent_checkpoint_id.map(|value| value.to_string()),
                task_state_json
            ],
        ),
    }
    .map_err(|source| database_error("записать чат", database_path, source))?;
    if matches!(mode, WriteMode::IgnoreExisting) && changed == 0 {
        return Ok(false);
    }

    transaction
        .execute("DELETE FROM messages WHERE chat_id = ?1", params![id])
        .map_err(|source| database_error("обновить сообщения", database_path, source))?;
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO messages(
                     chat_id, sequence, role, content, response_metrics_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|source| database_error("подготовить сообщения", database_path, source))?;
        for (sequence, message) in chat.messages.iter().enumerate() {
            let metrics_json = message
                .metrics
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|source| ChatStoreError::MessageMetricsJson {
                    action: "сохранить",
                    id: chat.id,
                    sequence,
                    source,
                })?;
            statement
                .execute(params![
                    id,
                    i64::try_from(sequence)
                        .map_err(|_| ChatStoreError::InvalidConversation(chat.id))?,
                    message.role.as_api_str(),
                    message.content,
                    metrics_json,
                ])
                .map_err(|source| database_error("записать сообщение", database_path, source))?;
        }
    }
    transaction
        .execute("DELETE FROM working_memory WHERE chat_id = ?1", params![id])
        .map_err(|source| database_error("обновить рабочую память", database_path, source))?;
    {
        let mut statement = transaction
            .prepare("INSERT INTO working_memory(chat_id, key, value) VALUES (?1, ?2, ?3)")
            .map_err(|source| {
                database_error("подготовить рабочую память", database_path, source)
            })?;
        for (key, value) in &chat.working_memory {
            statement
                .execute(params![id, key, value])
                .map_err(|source| {
                    database_error("записать рабочую память", database_path, source)
                })?;
        }
    }
    transaction
        .execute(
            "INSERT INTO memory_settings(chat_id, profile_enabled, profile_name) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id) DO UPDATE SET
                 profile_enabled = excluded.profile_enabled,
                 profile_name = excluded.profile_name",
            params![
                id,
                chat.memory_selection.profile,
                chat.memory_selection.profile_name
            ],
        )
        .map_err(|source| database_error("записать настройки памяти", database_path, source))?;
    transaction
        .execute(
            "DELETE FROM memory_selections WHERE chat_id = ?1",
            params![id],
        )
        .map_err(|source| database_error("обновить подключения памяти", database_path, source))?;
    {
        let mut statement = transaction
            .prepare("INSERT INTO memory_selections(chat_id, kind, name) VALUES (?1, ?2, ?3)")
            .map_err(|source| {
                database_error("подготовить подключения памяти", database_path, source)
            })?;
        for reference in &chat.memory_selection.entries {
            statement
                .execute(params![id, reference.kind.to_string(), reference.name])
                .map_err(|source| {
                    database_error("записать подключения памяти", database_path, source)
                })?;
        }
    }
    Ok(true)
}

fn initialize_database(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), ChatStoreError> {
    connection
        .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
        .map_err(|source| database_error("настроить", database_path, source))?;
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|source| database_error("прочитать версию", database_path, source))?;
    match version {
        0 => connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS chats (
                     id TEXT PRIMARY KEY NOT NULL,
                     title TEXT NOT NULL,
                     created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
                     updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
                     settings_json TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS messages (
                     chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                     sequence INTEGER NOT NULL CHECK(sequence >= 0),
                     role TEXT NOT NULL CHECK(role IN ('user', 'assistant')),
                     content TEXT NOT NULL,
                     response_metrics_json TEXT,
                     PRIMARY KEY(chat_id, sequence)
                 );
                 CREATE TABLE IF NOT EXISTS metadata (
                     key TEXT PRIMARY KEY NOT NULL,
                     value TEXT NOT NULL
                 );
                 PRAGMA user_version = 2;
                 COMMIT;",
            )
            .map_err(|source| database_error("создать схему", database_path, source)),
        1 => connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 ALTER TABLE messages ADD COLUMN response_metrics_json TEXT;
                 PRAGMA user_version = 2;
                 COMMIT;",
            )
            .map_err(|source| database_error("обновить схему", database_path, source)),
        2..=DATABASE_SCHEMA_VERSION => Ok(()),
        value => Err(ChatStoreError::UnsupportedSchema(value)),
    }?;
    if version < 3 {
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS agents (
                 id TEXT PRIMARY KEY NOT NULL,
                 handle TEXT NOT NULL COLLATE NOCASE UNIQUE,
                 name TEXT NOT NULL,
                 description TEXT NOT NULL,
                 settings_json TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             PRAGMA user_version = 3;
             COMMIT;",
            )
            .map_err(|source| database_error("добавить каталог агентов", database_path, source))?;
    }
    if version < 4 {
        connection.execute_batch("BEGIN IMMEDIATE; ALTER TABLE chats ADD COLUMN summary_json TEXT; PRAGMA user_version = 4; COMMIT;")
            .map_err(|source| database_error("добавить резюме", database_path, source))?;
    }
    if version < 5 {
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 ALTER TABLE chats ADD COLUMN facts_json TEXT NOT NULL DEFAULT '{}';
                 ALTER TABLE chats ADD COLUMN branch_group_id TEXT;
                 ALTER TABLE chats ADD COLUMN branch_name TEXT;
                 ALTER TABLE chats ADD COLUMN parent_checkpoint_id TEXT;
                 UPDATE chats SET branch_group_id = id, branch_name = 'main'
                   WHERE branch_group_id IS NULL OR branch_name IS NULL;
                 CREATE UNIQUE INDEX IF NOT EXISTS chats_branch_name
                   ON chats(branch_group_id, branch_name COLLATE NOCASE);
                 CREATE TABLE IF NOT EXISTS checkpoints (
                     id TEXT PRIMARY KEY NOT NULL,
                     branch_group_id TEXT NOT NULL,
                     name TEXT NOT NULL COLLATE NOCASE,
                     source_chat_id TEXT NOT NULL,
                     created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
                     snapshot_json TEXT NOT NULL,
                     UNIQUE(branch_group_id, name)
                 );
                 PRAGMA user_version = 5;
                 COMMIT;",
            )
            .map_err(|source| {
                database_error("добавить стратегии контекста", database_path, source)
            })?;
    }
    if version < 6 {
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS working_memory (
                     chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                     key TEXT NOT NULL,
                     value TEXT NOT NULL,
                     PRIMARY KEY(chat_id, key)
                 );
                 CREATE TABLE IF NOT EXISTS memory_settings (
                     chat_id TEXT PRIMARY KEY NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                     profile_enabled INTEGER NOT NULL DEFAULT 1 CHECK(profile_enabled IN (0, 1)),
                     profile_name TEXT NOT NULL DEFAULT 'default'
                 );
                 CREATE TABLE IF NOT EXISTS memory_selections (
                     chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
                     kind TEXT NOT NULL CHECK(kind IN ('decision', 'knowledge')),
                     name TEXT NOT NULL,
                     PRIMARY KEY(chat_id, kind, name)
                 );
                 PRAGMA user_version = 6;
                 COMMIT;",
            )
            .map_err(|source| database_error("добавить слои памяти", database_path, source))?;
    }
    if version < 7 {
        let has_profile_name = connection
            .prepare("PRAGMA table_info(memory_settings)")
            .and_then(|mut statement| {
                let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
                for column in columns {
                    if column.as_deref() == Ok("profile_name") {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .map_err(|source| database_error("проверить схему профилей", database_path, source))?;
        if has_profile_name {
            connection
                .execute_batch("PRAGMA user_version = 7;")
                .map_err(|source| {
                    database_error("обновить версию профилей", database_path, source)
                })?;
        } else {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE memory_settings ADD COLUMN profile_name TEXT NOT NULL DEFAULT 'default';
                     PRAGMA user_version = 7;
                     COMMIT;",
                )
                .map_err(|source| database_error("добавить именованные профили", database_path, source))?;
        }
    }
    if version < 8 {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('chats') WHERE name = 'task_state_json')", [], |row| row.get(0)
        ).map_err(|source| database_error("проверить схему задач", database_path, source))?;
        let migration = if exists {
            "PRAGMA user_version = 8;"
        } else {
            "BEGIN IMMEDIATE; ALTER TABLE chats ADD COLUMN task_state_json TEXT; PRAGMA user_version = 8; COMMIT;"
        };
        connection
            .execute_batch(migration)
            .map_err(|source| database_error("добавить состояние задачи", database_path, source))?;
    }
    if version < 9 {
        connection
            .execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS invariants (
                     name TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
                     rule TEXT NOT NULL
                 );
                 PRAGMA user_version = 9;
                 COMMIT;",
            )
            .map_err(|source| database_error("добавить инварианты", database_path, source))?;
    }
    Ok(())
}

fn read_legacy_chat(path: &Path) -> Option<Chat> {
    if path.extension().and_then(|value| value.to_str()) != Some("json") {
        return None;
    }
    let expected_id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .and_then(|value| Uuid::parse_str(value).ok())?;
    let mut chat: Chat = serde_json::from_reader(File::open(path).ok()?).ok()?;
    if chat.schema_version != LEGACY_CHAT_SCHEMA_VERSION
        || chat.id != expected_id
        || !valid_messages(&chat.messages)
    {
        return None;
    }
    chat.mark_loaded();
    Some(chat)
}

fn valid_messages(messages: &[ChatMessage]) -> bool {
    let (pairs, remainder) = messages.as_chunks::<2>();
    !pairs.is_empty()
        && remainder.is_empty()
        && pairs.iter().all(|pair| {
            pair[0].role == MessageRole::User
                && pair[0].metrics.is_none()
                && pair[1].role == MessageRole::Assistant
        })
}

fn validate_context_name(name: &str, allow_main: bool) -> Result<(), String> {
    let valid = (1..=32).contains(&name.len())
        && name.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        });
    if !valid {
        return Err("Имя должно содержать 1–32 ASCII-символа: буквы, цифры, '_' или '-'.".into());
    }
    if !allow_main && name.eq_ignore_ascii_case("main") {
        return Err("Имя ветки main зарезервировано.".into());
    }
    Ok(())
}

fn title_from_question(question: &str) -> String {
    let normalized = question.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let title = characters
        .by_ref()
        .take(MAX_TITLE_CHARS.saturating_sub(1))
        .collect::<String>();
    if characters.next().is_some() {
        format!("{title}…")
    } else if title.is_empty() {
        "Новый чат".to_owned()
    } else {
        title
    }
}

fn timestamp_for_database(id: Uuid, timestamp: u64) -> Result<i64, ChatStoreError> {
    i64::try_from(timestamp).map_err(|_| ChatStoreError::InvalidTimestamp(id))
}

fn now_millis() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    millis.min(u128::from(u64::MAX)) as u64
}

fn state_directory() -> Option<PathBuf> {
    state_directory_from_lookup(|name| env::var_os(name))
}

fn state_directory_from_lookup<F>(mut lookup: F) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<OsString>,
{
    if let Some(state_dir) = lookup("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(state_dir).join("agi"));
    }
    if let Some(home_dir) = lookup("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home_dir).join(".local/state/agi"));
    }
    lookup("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("agi"))
}

fn database_error(action: &'static str, path: &Path, source: rusqlite::Error) -> ChatStoreError {
    ChatStoreError::Database {
        action,
        path: path.to_owned(),
        source,
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), ChatStoreError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        ChatStoreError::Io {
            action: "ограничить доступ к каталогу",
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), ChatStoreError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), ChatStoreError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        ChatStoreError::Io {
            action: "ограничить доступ к базе",
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), ChatStoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::input::BufferedInput;
    use crate::metrics::TokenUsage;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            Self(env::temp_dir().join(format!("agi-chat-test-{}", Uuid::new_v4())))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn global_invariants_survive_reopen_and_are_shared_by_chats() {
        let directory = TestDirectory::new();
        let first = ChatStore::for_tests(directory.0.clone()).expect("store");
        first
            .invariants()
            .set("stack", "Только Rust")
            .expect("set invariant");
        let one = Chat::new();
        let two = Chat::new();
        assert_ne!(one.id(), two.id());
        assert_eq!(first.invariants().list().expect("list").len(), 1);
        drop(first);

        let reopened = ChatStore::for_tests(directory.0.clone()).expect("reopen");
        assert_eq!(
            reopened.invariants().list().expect("list"),
            vec![crate::invariants::Invariant {
                name: "stack".into(),
                rule: "Только Rust".into(),
            }]
        );
        assert_eq!(
            reopened
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("version"),
            DATABASE_SCHEMA_VERSION
        );
    }

    #[test]
    fn migrates_v8_database_to_global_invariants() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        store
            .connection
            .execute_batch("DROP TABLE invariants; PRAGMA user_version = 8;")
            .expect("v8 fixture");
        drop(store);

        let migrated = ChatStore::for_tests(directory.0.clone()).expect("migrate");
        assert!(migrated.invariants().list().expect("list").is_empty());
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("version"),
            DATABASE_SCHEMA_VERSION
        );
    }

    #[test]
    fn migrates_v7_and_preserves_task_only_chats() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).unwrap();
        let mut legacy = Chat::new();
        legacy.record_exchange("Вопрос".into(), "Ответ".into());
        store.save(&mut legacy).unwrap();
        store
            .connection
            .execute_batch(
                "ALTER TABLE chats DROP COLUMN task_state_json; PRAGMA user_version = 7;",
            )
            .unwrap();
        drop(store);
        let store = ChatStore::for_tests(directory.0.clone()).unwrap();
        let loaded = store.load(legacy.id()).unwrap();
        assert!(loaded.task().is_none());
        assert_eq!(loaded.messages(), legacy.messages());
        let mut task_only = Chat::new();
        crate::task::command(
            &store,
            &mut task_only,
            Some("start Сохранить до первого API-запроса"),
        )
        .unwrap();
        let loaded = store.load(task_only.id()).unwrap();
        assert!(loaded.messages().is_empty());
        assert!(loaded.task().unwrap().paused, "restore never auto-runs");
        assert_eq!(
            loaded.task().unwrap().stage,
            crate::task::TaskStage::Planning
        );
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            DATABASE_SCHEMA_VERSION
        );
    }

    #[test]
    fn failed_task_step_commit_rolls_back_answer_and_state() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).unwrap();
        let mut chat = Chat::new();
        crate::task::command(&store, &mut chat, Some("start Описание проекта")).unwrap();
        let before = chat.task().unwrap().clone();
        let planned = before
            .apply(
                crate::task::TaskUpdate {
                    operation: crate::task::TaskOperation::Plan,
                    steps: vec!["Текст".into()],
                    question: String::new(),
                    reason: String::new(),
                },
                "План: написать текст",
            )
            .unwrap();
        // Fails after the chat UPDATE, while inserting the answer in the same transaction.
        store.connection.execute_batch("CREATE TRIGGER reject_messages BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT, 'test disk error'); END;").unwrap();
        let result = crate::task::commit_answer(
            &store,
            &mut chat,
            "Составь план".into(),
            crate::agent::AgentAnswer {
                content: "План: написать текст".into(),
                truncated: false,
                elapsed_ms: 1,
                calls: vec![],
                already_counted_usage: None,
                updated_facts: None,
                updated_task: Some(planned),
                invariant_refusal: false,
            },
            &PriceCatalog::default(),
            &mut crate::task::RunBudget::default(),
        );
        assert!(result.is_err());
        assert_eq!(chat.task(), Some(&before));
        assert!(chat.messages().is_empty());
        let loaded = store.load(chat.id()).unwrap();
        assert!(loaded.task().unwrap().steps.is_empty());
        assert!(loaded.messages().is_empty());
    }

    #[test]
    fn task_survives_summary_and_checkpoint_branches_are_independent() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).unwrap();
        let mut chat = Chat::new();
        chat.settings_mut()
            .select_context_strategy(ContextStrategyKind::Branching);
        crate::task::command(&store, &mut chat, Some("start Проект")).unwrap();
        chat.record_exchange("План".into(), "Описание плана".into());
        crate::task::pause(&store, &mut chat, "Пауза").unwrap();
        let original = chat.task().cloned();
        store.create_checkpoint(&chat, "snapshot").unwrap();
        let mut branch = store
            .create_branch(chat.branch_group_id(), "snapshot", "experiment")
            .unwrap();
        assert_eq!(branch.task(), original.as_ref());
        crate::task::command(&store, &mut branch, Some("resume")).unwrap();
        crate::task::prepare_input(&store, &mut branch, "Уточнение только для ветки").unwrap();
        assert_eq!(store.load(chat.id()).unwrap().task(), original.as_ref());
        store
            .replace_with_summary(
                &mut chat,
                crate::summary::ConversationSummary {
                    content: "Короткое резюме".into(),
                    replaced_messages: 2,
                    before_bytes: 100,
                    metrics: ResponseMetrics::new("qwen3.8-27b", 0, None, &PriceCatalog::default()),
                },
            )
            .unwrap();
        assert_eq!(chat.task(), original.as_ref());
        assert_eq!(store.load(chat.id()).unwrap().task(), original.as_ref());
        assert!(chat.messages().is_empty());
    }

    #[test]
    fn migrates_version_three_without_changing_messages_or_agents() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();
        chat.record_exchange("Вопрос".into(), "Ответ".into());
        store.save(&mut chat).expect("save");
        store
            .connection
            .execute_batch(
                "DROP TABLE checkpoints;
                 DROP INDEX chats_branch_name;
                 ALTER TABLE chats DROP COLUMN parent_checkpoint_id;
                 ALTER TABLE chats DROP COLUMN branch_name;
                 ALTER TABLE chats DROP COLUMN branch_group_id;
                 ALTER TABLE chats DROP COLUMN facts_json;
                 ALTER TABLE chats DROP COLUMN summary_json;
                 PRAGMA user_version = 3;",
            )
            .expect("v3 fixture");
        drop(store);
        let migrated = ChatStore::for_tests(directory.0.clone()).expect("migrate");
        let restored = migrated.load(chat.id()).expect("load");
        assert_eq!(restored.messages(), chat.messages());
        assert!(restored.summary().is_none());
        assert!(migrated.agents().list().is_ok());
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("version"),
            DATABASE_SCHEMA_VERSION
        );
    }

    #[test]
    fn summary_replaces_persisted_messages_atomically_and_preserves_identity() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();
        chat.record_exchange("Старый вопрос".into(), "Старый ответ".into());
        store.save(&mut chat).expect("save");
        let id = chat.id();
        let title = chat.title().to_owned();
        let summary = crate::summary::ConversationSummary {
            content: "Резюме".into(),
            replaced_messages: 2,
            before_bytes: 100,
            metrics: ResponseMetrics::new("main", 1, None, &PriceCatalog::default()),
        };
        store.connection.execute_batch("CREATE TRIGGER reject_delete BEFORE DELETE ON messages BEGIN SELECT RAISE(ABORT, 'test failure'); END;").expect("trigger");
        assert!(
            store
                .replace_with_summary(&mut chat, summary.clone())
                .is_err()
        );
        assert_eq!(chat.messages().len(), 2);
        assert!(chat.summary().is_none());
        assert_eq!(store.load(id).expect("load").messages().len(), 2);
        store
            .connection
            .execute_batch("DROP TRIGGER reject_delete;")
            .expect("remove trigger");
        store
            .replace_with_summary(&mut chat, summary)
            .expect("replace");
        store.connection.execute("UPDATE chats SET summary_json = json_remove(summary_json, '$.metrics.is_summary') WHERE id = ?1", params![id.to_string()]).expect("legacy summary fixture");
        let mut restored = store.load(id).expect("summary-only chat");
        assert!(
            restored
                .last_response_metrics()
                .expect("summary metrics")
                .is_summary
        );
        assert!(restored.messages().is_empty());
        assert_eq!(restored.summary().expect("summary").content, "Резюме");
        assert!(restored.has_completed_turn());
        assert_eq!(restored.title(), title);
        assert_eq!(restored.settings(), chat.settings());
        restored.record_exchange("Новый вопрос".into(), "Ответ".into());
        assert_eq!(restored.title(), title);
        assert_eq!(restored.id(), id);
        store.save(&mut restored).expect("save next turn");
        assert_eq!(store.load(id).expect("load").messages().len(), 2);
        assert!(Chat::new().summary().is_none());
    }

    #[test]
    fn accumulates_every_api_usage_across_turns_and_summary_without_duplicates() {
        use crate::metrics::{CallUsage, TokenUsage};

        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let make_metrics = |prompt_tokens, completion_tokens| {
            let usage = TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_prompt_tokens: 0,
            };
            ResponseMetrics::from_calls(
                "qwen3.8-27b",
                0,
                vec![CallUsage {
                    context: None,
                    model: "qwen3.8-27b".into(),
                    usage: Some(usage),
                }],
                &PriceCatalog::default(),
            )
        };
        let mut chat = Chat::new();
        chat.record_exchange_with_metrics(
            "Первый".into(),
            "Ответ".into(),
            Some(make_metrics(70, 30)),
        );
        chat.record_exchange_with_metrics(
            "Второй".into(),
            "Ответ".into(),
            Some(make_metrics(150, 50)),
        );
        assert_eq!(
            chat.cumulative_api_usage(),
            Some(TokenUsage {
                prompt_tokens: 220,
                completion_tokens: 80,
                total_tokens: 300,
                cached_prompt_tokens: 0,
            })
        );

        let summary_metrics = make_metrics(35, 15);
        let summary_calls = summary_metrics.calls.clone();
        store
            .replace_with_summary(
                &mut chat,
                crate::summary::ConversationSummary {
                    content: "Резюме".into(),
                    replaced_messages: 4,
                    before_bytes: 100,
                    metrics: summary_metrics,
                },
            )
            .expect("summary");
        assert_eq!(
            chat.cumulative_api_usage()
                .expect("summary usage")
                .total_tokens,
            350
        );

        let mut calls = summary_calls;
        calls.extend(make_metrics(20, 5).calls);
        let mut combined =
            ResponseMetrics::from_calls("qwen3.8-27b", 0, calls, &PriceCatalog::default());
        combined.already_counted_usage = Some(TokenUsage {
            prompt_tokens: 35,
            completion_tokens: 15,
            total_tokens: 50,
            cached_prompt_tokens: 0,
        });
        chat.record_exchange_with_metrics("После резюме".into(), "Ответ".into(), Some(combined));
        assert_eq!(
            chat.cumulative_api_usage(),
            Some(TokenUsage {
                prompt_tokens: 275,
                completion_tokens: 100,
                total_tokens: 375,
                cached_prompt_tokens: 0,
            })
        );
        store.save(&mut chat).expect("save");
        assert_eq!(
            store
                .load(chat.id())
                .expect("restore")
                .cumulative_api_usage(),
            chat.cumulative_api_usage()
        );
    }

    #[test]
    fn saves_only_after_complete_exchange_and_restores_settings() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store should open");
        let mut chat = Chat::new();
        let id = chat.id();
        let mut settings_input = BufferedInput::new(Cursor::new(
            "1\n3\n6\n1\nОтвечай как редактор\n3\n900\n4\n1.25\nesc\n",
        ));
        chat.settings_mut()
            .configure(&mut settings_input, &mut Vec::new())
            .expect("settings should change");
        chat.mark_changed();

        assert!(!store.save(&mut chat).expect("empty chat should not fail"));
        assert!(store.list().expect("list should load").chats.is_empty());

        let metrics = ResponseMetrics {
            is_summary: false,
            model: "gpt-oss-120b".to_owned(),
            elapsed_ms: 1_234,
            usage: Some(TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 25,
                total_tokens: 125,
                cached_prompt_tokens: 40,
            }),
            estimated_cost_microrubles: Some(1_020),
            premium: Some(false),
            calls: Vec::new(),
            cumulative_usage: None,
            already_counted_usage: None,
        };
        chat.record_exchange_with_metrics(
            "  Первый   вопрос  ".to_owned(),
            "Первый ответ".to_owned(),
            Some(metrics.clone()),
        );
        assert!(store.save(&mut chat).expect("chat should save"));
        let restored = store.load(id).expect("chat should restore");

        assert_eq!(restored.title(), "Первый вопрос");
        assert_eq!(restored.messages(), chat.messages());
        assert_eq!(restored.settings(), chat.settings());
        assert_eq!(
            restored.settings().system_prompt(),
            Some("Отвечай как редактор")
        );
        assert_eq!(restored.settings().model(), "gpt-oss-120b");
        assert_eq!(restored.settings().max_tokens(), 900);
        assert_eq!(restored.settings().temperature(), 1.25);
        assert_eq!(
            restored.last_response_metrics(),
            chat.last_response_metrics()
        );
        assert!(restored.is_persisted());
        assert!(!restored.is_dirty());
        assert!(directory.0.join(DATABASE_FILE_NAME).is_file());
    }

    #[test]
    fn migrates_version_two_and_preserves_chat_and_per_model_metrics() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();
        let metrics = ResponseMetrics::from_calls(
            "qwen3.8-27b",
            1200,
            vec![
                crate::metrics::CallUsage {
                    context: None,
                    model: "gpt-oss-20b".into(),
                    usage: Some(TokenUsage {
                        total_tokens: 15,
                        ..TokenUsage::default()
                    }),
                },
                crate::metrics::CallUsage {
                    context: None,
                    model: "qwen3.8-27b".into(),
                    usage: Some(TokenUsage {
                        total_tokens: 20,
                        ..TokenUsage::default()
                    }),
                },
            ],
            &PriceCatalog::default(),
        );
        chat.record_exchange_with_metrics("Вопрос".into(), "Итог".into(), Some(metrics.clone()));
        store.save(&mut chat).expect("save");
        store
            .connection
            .execute_batch(
                "DROP TABLE agents;
                 DROP TABLE checkpoints;
                 DROP INDEX chats_branch_name;
                 ALTER TABLE chats DROP COLUMN parent_checkpoint_id;
                 ALTER TABLE chats DROP COLUMN branch_name;
                 ALTER TABLE chats DROP COLUMN branch_group_id;
                 ALTER TABLE chats DROP COLUMN facts_json;
                 ALTER TABLE chats DROP COLUMN summary_json;
                 PRAGMA user_version = 2;",
            )
            .expect("v2 fixture");
        drop(store);
        let migrated = ChatStore::for_tests(directory.0.clone()).expect("migrate");
        assert!(migrated.agents().list().expect("catalog").is_empty());
        let restored = migrated.load(chat.id()).expect("restore");
        assert_eq!(restored.messages(), chat.messages());
        assert_eq!(
            restored.last_response_metrics(),
            chat.last_response_metrics()
        );
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("version"),
            DATABASE_SCHEMA_VERSION
        );
    }

    #[test]
    fn lists_newest_chats_first_and_skips_invalid_rows() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store should open");
        let mut older = Chat::new();
        older.record_exchange("Старый".to_owned(), "Ответ".to_owned());
        older.updated_at_ms = 10;
        store.save(&mut older).expect("older chat should save");
        let mut newer = Chat::new();
        newer.record_exchange("Новый".to_owned(), "Ответ".to_owned());
        newer.updated_at_ms = 20;
        store.save(&mut newer).expect("newer chat should save");
        store
            .connection
            .execute(
                "INSERT INTO chats(id, title, created_at_ms, updated_at_ms, settings_json)
                 VALUES ('not-a-uuid', 'Повреждённый', 1, 30, '{}')",
                [],
            )
            .expect("invalid fixture should be inserted");

        let list = store.list().expect("list should load");

        assert_eq!(list.chats.len(), 2);
        assert_eq!(list.chats[0].id, newer.id());
        assert_eq!(list.chats[1].id, older.id());
        assert_eq!(list.skipped_entries, 1);
    }

    #[test]
    fn imports_legacy_json_only_once() {
        let directory = TestDirectory::new();
        let legacy_directory = directory.0.join(LEGACY_DIRECTORY_NAME);
        fs::create_dir_all(&legacy_directory).expect("legacy directory should be created");
        let mut legacy = Chat::new();
        legacy.record_exchange("Старый вопрос".to_owned(), "Старый ответ".to_owned());
        let id = legacy.id();
        fs::write(
            legacy_directory.join(format!("{id}.json")),
            serde_json::to_vec_pretty(&legacy).expect("legacy JSON should serialize"),
        )
        .expect("legacy JSON should be written");

        {
            let store = ChatStore::for_tests(directory.0.clone()).expect("store should import");
            let mut imported = store.load(id).expect("imported chat should load");
            imported.record_exchange("Новый вопрос".to_owned(), "Новый ответ".to_owned());
            store
                .save(&mut imported)
                .expect("imported chat should update");
        }
        let reopened = ChatStore::for_tests(directory.0.clone()).expect("store should reopen");
        let imported = reopened.load(id).expect("updated chat should load");

        assert_eq!(imported.messages().len(), 4);
        assert!(legacy_directory.join(format!("{id}.json")).is_file());
    }

    #[test]
    fn reports_missing_chat() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store should open");
        let id = Uuid::new_v4();

        assert!(matches!(store.load(id), Err(ChatStoreError::NotFound(value)) if value == id));
    }

    #[test]
    fn rejects_newer_database_schema() {
        let directory = TestDirectory::new();
        fs::create_dir_all(&directory.0).expect("state directory should be created");
        let connection = Connection::open(directory.0.join(DATABASE_FILE_NAME))
            .expect("fixture database should open");
        connection
            .execute_batch("PRAGMA user_version = 99;")
            .expect("fixture version should be set");
        drop(connection);

        let error =
            ChatStore::for_tests(directory.0.clone()).expect_err("newer schema should be rejected");

        assert!(matches!(error, ChatStoreError::UnsupportedSchema(99)));
    }

    #[test]
    fn migrates_database_schema_from_v1() {
        let directory = TestDirectory::new();
        fs::create_dir_all(&directory.0).expect("state directory should be created");
        let database_path = directory.0.join(DATABASE_FILE_NAME);
        let connection = Connection::open(&database_path).expect("fixture database should open");
        connection
            .execute_batch(
                "CREATE TABLE chats (
                     id TEXT PRIMARY KEY NOT NULL,
                     title TEXT NOT NULL,
                     created_at_ms INTEGER NOT NULL,
                     updated_at_ms INTEGER NOT NULL,
                     settings_json TEXT NOT NULL
                 );
                 CREATE TABLE messages (
                     chat_id TEXT NOT NULL,
                     sequence INTEGER NOT NULL,
                     role TEXT NOT NULL,
                     content TEXT NOT NULL,
                     PRIMARY KEY(chat_id, sequence)
                 );
                 CREATE TABLE metadata (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                 PRAGMA user_version = 1;",
            )
            .expect("v1 fixture should be created");
        drop(connection);

        let store = ChatStore::for_tests(directory.0.clone()).expect("v1 database should migrate");
        let version = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("schema version should be readable");
        let has_metrics_column = store
            .connection
            .prepare("PRAGMA table_info(messages)")
            .expect("table info should prepare")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("columns should be readable")
            .filter_map(Result::ok)
            .any(|column| column == "response_metrics_json");

        assert_eq!(version, DATABASE_SCHEMA_VERSION);
        assert!(has_metrics_column);
    }

    #[test]
    fn migrates_profile_selection_from_v6_to_named_default() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();
        chat.select_profile("senior".into());
        store.save(&mut chat).expect("save profile selection");
        store
            .connection
            .execute_batch(
                "ALTER TABLE memory_settings DROP COLUMN profile_name;
                 PRAGMA user_version = 6;",
            )
            .expect("v6 fixture");
        drop(store);

        let migrated = ChatStore::for_tests(directory.0.clone()).expect("migrate v6");
        let restored = migrated.load(chat.id()).expect("restore selection");
        assert!(restored.memory_selection().profile);
        assert_eq!(
            restored.memory_selection().profile_name,
            crate::memory::DEFAULT_PROFILE_NAME
        );
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .expect("schema version"),
            DATABASE_SCHEMA_VERSION
        );
    }

    #[test]
    fn shortens_long_title_on_character_boundary() {
        let question = "я".repeat(100);
        let title = title_from_question(&question);

        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn sliding_window_discards_old_exchanges() {
        let mut chat = Chat::new();
        chat.settings_mut().set_context_window("2").expect("window");
        for index in 1..=3 {
            chat.record_exchange(format!("Вопрос {index}"), format!("Ответ {index}"));
        }
        assert_eq!(chat.messages().len(), 2);
        assert_eq!(chat.messages()[0].content, "Вопрос 3");
        assert_eq!(chat.messages()[1].content, "Ответ 3");
    }

    #[test]
    fn sticky_facts_and_window_are_persisted() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();
        chat.settings_mut()
            .select_context_strategy(ContextStrategyKind::StickyFacts);
        chat.settings_mut().set_context_window("2").expect("window");
        chat.set_fact("goal".into(), "Собрать CLI".into())
            .expect("fact");
        chat.record_exchange("Первый".into(), "Один".into());
        chat.record_exchange("Второй".into(), "Два".into());
        store.save(&mut chat).expect("save");
        let restored = store.load(chat.id()).expect("load");
        assert_eq!(
            restored.facts().get("goal").map(String::as_str),
            Some("Собрать CLI")
        );
        assert_eq!(restored.messages().len(), 2);
        assert_eq!(restored.messages()[0].content, "Второй");
    }

    #[test]
    fn creates_independent_branches_from_one_checkpoint() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut main = Chat::new();
        main.settings_mut()
            .select_context_strategy(ContextStrategyKind::Branching);
        main.record_exchange("Основа".into(), "Принято".into());
        main.set_working("step".into(), "planning".into())
            .expect("working memory");
        main.use_memory(MemoryRef {
            kind: crate::memory::LongTermKind::Decision,
            name: "architecture".into(),
        });
        main.select_profile("senior".into());
        store.save(&mut main).expect("save main");
        store.create_checkpoint(&main, "base").expect("checkpoint");

        let mut first = store
            .create_branch(main.branch_group_id(), "base", "first")
            .expect("first branch");
        assert_eq!(
            first.working_memory().get("step").map(String::as_str),
            Some("planning")
        );
        assert_eq!(first.memory_selection().entries.len(), 1);
        assert_eq!(first.memory_selection().profile_name, "senior");
        first.select_profile("beginner".into());
        first
            .set_working("step".into(), "execution".into())
            .expect("independent working memory");
        first.record_exchange("Путь A".into(), "Ответ A".into());
        store.save(&mut first).expect("save first");
        let mut second = store
            .create_branch(main.branch_group_id(), "base", "second")
            .expect("second branch");
        second.record_exchange("Путь B".into(), "Ответ B".into());
        store.save(&mut second).expect("save second");

        let first = store
            .load_branch(main.branch_group_id(), "first")
            .expect("load first");
        let second = store
            .load_branch(main.branch_group_id(), "second")
            .expect("load second");
        assert_eq!(
            first.working_memory().get("step").map(String::as_str),
            Some("execution")
        );
        assert_eq!(
            second.working_memory().get("step").map(String::as_str),
            Some("planning")
        );
        assert_eq!(first.memory_selection().profile_name, "beginner");
        assert_eq!(second.memory_selection().profile_name, "senior");
        assert!(
            first
                .messages()
                .iter()
                .any(|message| message.content == "Путь A")
        );
        assert!(
            !first
                .messages()
                .iter()
                .any(|message| message.content == "Путь B")
        );
        assert!(
            second
                .messages()
                .iter()
                .any(|message| message.content == "Путь B")
        );
        assert_eq!(
            store
                .branches(main.branch_group_id())
                .expect("branches")
                .len(),
            3
        );
        assert_eq!(
            store
                .checkpoints(main.branch_group_id())
                .expect("checkpoints")
                .len(),
            1
        );
    }

    #[test]
    fn chooses_platform_state_directory() {
        let path = state_directory_from_lookup(|name| match name {
            "XDG_STATE_HOME" => Some(OsString::from("/state")),
            "HOME" => Some(OsString::from("/home/user")),
            _ => None,
        });

        assert_eq!(path, Some(PathBuf::from("/state/agi")));
    }
}
