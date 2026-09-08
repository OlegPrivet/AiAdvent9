use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::settings::Settings;

pub(crate) const MAX_AGENTS: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct AgentDefinition {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) handle: String,
    pub(crate) description: String,
    pub(crate) settings: Settings,
}

impl AgentDefinition {
    pub(crate) fn draft() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            handle: String::new(),
            description: String::new(),
            settings: Settings::default(),
        }
    }

    pub(crate) fn label(&self) -> String {
        format!("{} (@{}) — {}", self.name, self.handle, self.description)
    }

    pub(crate) fn validate(&self) -> Result<(), CatalogError> {
        validate_text("Имя", &self.name, 80)?;
        validate_handle(&self.handle)?;
        validate_text("Описание", &self.description, 240)?;
        validate_text(
            "System prompt",
            self.settings.system_prompt().unwrap_or_default(),
            8000,
        )?;
        if self.settings.model().is_empty()
            || !self.settings.temperature().is_finite()
            || !(0.0..=2.0).contains(&self.settings.temperature())
            || self.settings.max_tokens() == 0
            || (self.settings.response_format_enabled() && self.settings.max_tokens() < 256)
        {
            return Err(CatalogError::Validation(
                "Некорректные LLM-настройки агента".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn validate_text(label: &str, value: &str, max: usize) -> Result<(), CatalogError> {
    if value.trim().is_empty() || value.chars().count() > max {
        return Err(CatalogError::Validation(format!(
            "{label}: требуется от 1 до {max} символов"
        )));
    }
    Ok(())
}

pub(crate) fn validate_handle(value: &str) -> Result<(), CatalogError> {
    if !(2..=32).contains(&value.len())
        || !value.starts_with(|c: char| c.is_ascii_lowercase())
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(CatalogError::Validation(
            "Handle: 2–32 символа a-z, 0-9, _ или -, первая — буква; без @".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("{0}")]
    Validation(String),
    #[error("ошибка SQLite каталога агентов: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("некорректные настройки сохранённого агента: {0}")]
    Json(#[from] serde_json::Error),
    #[error("некорректный UUID сохранённого агента: {0}")]
    Id(#[from] uuid::Error),
    #[error("агент больше не существует")]
    NotFound,
}

pub(crate) struct AgentStore<'a> {
    connection: &'a Connection,
}

impl<'a> AgentStore<'a> {
    pub(crate) fn new(connection: &'a Connection) -> Self {
        Self { connection }
    }

    pub(crate) fn list(&self) -> Result<Vec<AgentDefinition>, CatalogError> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, handle, description, settings_json FROM agents ORDER BY handle COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (id, name, handle, description, settings) = row?;
            let agent = AgentDefinition {
                id: Uuid::parse_str(&id)?,
                name,
                handle,
                description,
                settings: serde_json::from_str(&settings)?,
            };
            agent.validate()?;
            Ok(agent)
        })
        .collect()
    }

    pub(crate) fn get(&self, id: Uuid) -> Result<AgentDefinition, CatalogError> {
        self.list()?
            .into_iter()
            .find(|agent| agent.id == id)
            .ok_or(CatalogError::NotFound)
    }

    pub(crate) fn save(&self, agent: &AgentDefinition, creating: bool) -> Result<(), CatalogError> {
        let mut agent = agent.clone();
        agent.handle = agent.handle.trim().to_ascii_lowercase();
        agent.validate()?;
        let tx = self.connection.unchecked_transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT id FROM agents WHERE handle = ?1 COLLATE NOCASE",
                [&agent.handle],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some_and(|id| id != agent.id.to_string()) {
            return Err(CatalogError::Validation(format!(
                "Handle @{} уже занят",
                agent.handle
            )));
        }
        let settings = serde_json::to_string(&agent.settings)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        if creating {
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM agents", [], |row| row.get(0))?;
            if count >= MAX_AGENTS as i64 {
                return Err(CatalogError::Validation(format!("Можно создать не более {MAX_AGENTS} агентов")));
            }
            tx.execute("INSERT INTO agents(id, name, handle, description, settings_json, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![agent.id.to_string(), agent.name, agent.handle, agent.description, settings, now])?;
        } else if tx.execute("UPDATE agents SET name=?2, handle=?3, description=?4, settings_json=?5, updated_at_ms=?6 WHERE id=?1",
            params![agent.id.to_string(), agent.name, agent.handle, agent.description, settings, now])? == 0 {
            return Err(CatalogError::NotFound);
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn delete(&self, id: Uuid) -> Result<(), CatalogError> {
        if self
            .connection
            .execute("DELETE FROM agents WHERE id=?1", [id.to_string()])?
            == 0
        {
            return Err(CatalogError::NotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatStore;

    #[test]
    fn catalog_crud_is_global_and_validated() {
        let path = std::env::temp_dir().join(format!("agi-catalog-{}", Uuid::new_v4()));
        let store = ChatStore::for_tests(path.clone()).expect("store");
        let mut agent = AgentDefinition::draft();
        agent.name = "Редактор".into();
        agent.handle = "editor".into();
        agent.description = "Редактирует текст".into();
        agent
            .settings
            .set_system_prompt("Проверь текст".into())
            .expect("prompt");
        store.agents().save(&agent, true).expect("create");
        let reopened = ChatStore::for_tests(path.clone()).expect("reopen");
        assert_eq!(reopened.agents().get(agent.id).expect("read"), agent);
        let mut duplicate = agent.clone();
        duplicate.id = Uuid::new_v4();
        duplicate.handle = "EDITOR".into();
        assert!(store.agents().save(&duplicate, true).is_err());
        agent.description = "Новое описание".into();
        store.agents().save(&agent, false).expect("update");
        assert_eq!(store.agents().get(agent.id).expect("read"), agent);
        agent.handle = "bad handle".into();
        assert!(store.agents().save(&agent, false).is_err());
        store.agents().delete(agent.id).expect("delete");
        assert!(reopened.agents().list().expect("list").is_empty());
        drop(reopened);
        drop(store);
        std::fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn rejects_fifty_first_agent_and_invalid_lengths() {
        let path = std::env::temp_dir().join(format!("agi-limit-{}", Uuid::new_v4()));
        let db = ChatStore::for_tests(path.clone()).expect("store");
        let mut agent = AgentDefinition::draft();
        agent.name = "Агент".into();
        agent.description = "Описание".into();
        agent
            .settings
            .set_system_prompt("Инструкция".into())
            .expect("prompt");
        for index in 0..MAX_AGENTS {
            agent.id = Uuid::new_v4();
            agent.handle = format!("agent_{index}");
            db.agents().save(&agent, true).expect("create");
        }
        agent.id = Uuid::new_v4();
        agent.handle = "extra".into();
        assert!(db.agents().save(&agent, true).is_err());
        assert_eq!(db.agents().list().expect("list").len(), 50);
        agent.description = "д".repeat(241);
        assert!(agent.validate().is_err());
        assert!(validate_handle("1abc").is_err());
        assert!(validate_handle("a").is_err());
        drop(db);
        std::fs::remove_dir_all(path).expect("cleanup");
    }
}
