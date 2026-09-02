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

use crate::settings::Settings;

const LEGACY_CHAT_SCHEMA_VERSION: u32 = 1;
const DATABASE_SCHEMA_VERSION: i64 = 1;
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
    #[serde(skip)]
    persisted: bool,
    #[serde(skip)]
    dirty: bool,
}

impl Chat {
    pub(crate) fn new() -> Self {
        let now = now_millis();
        Self {
            schema_version: LEGACY_CHAT_SCHEMA_VERSION,
            id: Uuid::new_v4(),
            title: "Новый чат".to_owned(),
            created_at_ms: now,
            updated_at_ms: now,
            settings: Settings::default(),
            messages: Vec::new(),
            persisted: false,
            dirty: false,
        }
    }

    pub(crate) fn id(&self) -> Uuid {
        self.id
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

    pub(crate) fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    pub(crate) fn has_completed_turn(&self) -> bool {
        !self.messages.is_empty()
    }

    pub(crate) fn is_persisted(&self) -> bool {
        self.persisted
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn record_exchange(&mut self, question: String, answer: String) {
        if self.messages.is_empty() {
            self.title = title_from_question(&question);
        }
        self.messages.push(ChatMessage {
            role: MessageRole::User,
            content: question,
        });
        self.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            content: answer,
        });
        self.mark_changed();
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
    #[error("чат {0} содержит некорректную последовательность сообщений")]
    InvalidConversation(Uuid),
    #[error("чат {0} содержит некорректную временную метку")]
    InvalidTimestamp(Uuid),
}

impl ChatStore {
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
                "SELECT title, created_at_ms, updated_at_ms, settings_json
                 FROM chats WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(self.database_error("прочитать"))?;
        let Some((title, created_at_ms, updated_at_ms, settings_json)) = row else {
            return Err(ChatStoreError::NotFound(id));
        };
        let settings = serde_json::from_str(&settings_json).map_err(|source| {
            ChatStoreError::SettingsJson {
                action: "прочитать",
                id,
                source,
            }
        })?;

        let mut statement = self
            .connection
            .prepare(
                "SELECT sequence, role, content
                 FROM messages WHERE chat_id = ?1 ORDER BY sequence",
            )
            .map_err(|source| self.database_error_with_source("подготовить чтение", source))?;
        let rows = statement
            .query_map(params![id.to_string()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|source| self.database_error_with_source("прочитать сообщения", source))?;
        let mut messages = Vec::new();
        for (expected_sequence, row) in rows.enumerate() {
            let (sequence, role, content) = row
                .map_err(|source| self.database_error_with_source("прочитать сообщение", source))?;
            let Some(role) = MessageRole::from_database(&role) else {
                return Err(ChatStoreError::InvalidConversation(id));
            };
            if sequence != expected_sequence as i64 {
                return Err(ChatStoreError::InvalidConversation(id));
            }
            messages.push(ChatMessage { role, content });
        }
        if !valid_messages(&messages) {
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
            persisted: false,
            dirty: false,
        };
        chat.mark_loaded();
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

    pub(crate) fn save(&self, chat: &mut Chat) -> Result<bool, ChatStoreError> {
        if !chat.has_completed_turn() {
            return Ok(false);
        }
        if chat.is_persisted() && !chat.is_dirty() {
            return Ok(true);
        }
        if !valid_messages(&chat.messages) {
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
    let created_at_ms = timestamp_for_database(chat.id, chat.created_at_ms)?;
    let updated_at_ms = timestamp_for_database(chat.id, chat.updated_at_ms)?;
    let id = chat.id.to_string();

    let changed = match mode {
        WriteMode::Replace => transaction.execute(
            "INSERT INTO chats(id, title, created_at_ms, updated_at_ms, settings_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title,
                 created_at_ms = excluded.created_at_ms,
                 updated_at_ms = excluded.updated_at_ms,
                 settings_json = excluded.settings_json",
            params![id, chat.title, created_at_ms, updated_at_ms, settings_json],
        ),
        WriteMode::IgnoreExisting => transaction.execute(
            "INSERT OR IGNORE INTO chats(id, title, created_at_ms, updated_at_ms, settings_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, chat.title, created_at_ms, updated_at_ms, settings_json],
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
                "INSERT INTO messages(chat_id, sequence, role, content)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .map_err(|source| database_error("подготовить сообщения", database_path, source))?;
        for (sequence, message) in chat.messages.iter().enumerate() {
            statement
                .execute(params![
                    id,
                    i64::try_from(sequence)
                        .map_err(|_| ChatStoreError::InvalidConversation(chat.id))?,
                    message.role.as_api_str(),
                    message.content,
                ])
                .map_err(|source| database_error("записать сообщение", database_path, source))?;
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
                     PRIMARY KEY(chat_id, sequence)
                 );
                 CREATE TABLE IF NOT EXISTS metadata (
                     key TEXT PRIMARY KEY NOT NULL,
                     value TEXT NOT NULL
                 );
                 PRAGMA user_version = 1;
                 COMMIT;",
            )
            .map_err(|source| database_error("создать схему", database_path, source)),
        value if value == DATABASE_SCHEMA_VERSION => Ok(()),
        value => Err(ChatStoreError::UnsupportedSchema(value)),
    }
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
        && pairs
            .iter()
            .all(|pair| pair[0].role == MessageRole::User && pair[1].role == MessageRole::Assistant)
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
    fn saves_only_after_complete_exchange_and_restores_settings() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store should open");
        let mut chat = Chat::new();
        let id = chat.id();
        let mut settings_input =
            BufferedInput::new(Cursor::new("4\n1\nОтвечай как редактор\n2\n900\nesc\n"));
        chat.settings_mut()
            .configure(&mut settings_input, &mut Vec::new())
            .expect("settings should change");
        chat.mark_changed();

        assert!(!store.save(&mut chat).expect("empty chat should not fail"));
        assert!(store.list().expect("list should load").chats.is_empty());

        chat.record_exchange("  Первый   вопрос  ".to_owned(), "Первый ответ".to_owned());
        assert!(store.save(&mut chat).expect("chat should save"));
        let restored = store.load(id).expect("chat should restore");

        assert_eq!(restored.title(), "Первый вопрос");
        assert_eq!(restored.messages(), chat.messages());
        assert_eq!(restored.settings(), chat.settings());
        assert_eq!(
            restored.settings().system_prompt(),
            Some("Отвечай как редактор")
        );
        assert_eq!(restored.settings().max_tokens(), 900);
        assert!(restored.is_persisted());
        assert!(!restored.is_dirty());
        assert!(directory.0.join(DATABASE_FILE_NAME).is_file());
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
            .execute_batch("PRAGMA user_version = 2;")
            .expect("fixture version should be set");
        drop(connection);

        let error =
            ChatStore::for_tests(directory.0.clone()).expect_err("newer schema should be rejected");

        assert!(matches!(error, ChatStoreError::UnsupportedSchema(2)));
    }

    #[test]
    fn shortens_long_title_on_character_boundary() {
        let question = "я".repeat(100);
        let title = title_from_question(&question);

        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.ends_with('…'));
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
