//! Shared, offline connection wizard for the terminal and full-screen interfaces.
use std::io::{self, Write};

use crate::input::LineInput;
use crate::llm::{LlmError, LlmProfile, LlmStore, Provider};
use crate::ui::sanitize_terminal_text;

pub(crate) enum LlmPage {
    List {
        title: String,
        items: Vec<String>,
    },
    Text {
        title: String,
        value: String,
        hint: String,
    },
}

#[derive(Clone, Copy)]
enum Field {
    Id,
    Url,
    Model,
    Context,
}

enum Stage {
    Home,
    Card(usize),
    Field(Field),
    Tools,
    Schema,
    Confirm,
    Delete(usize),
}

pub(crate) struct LlmManager {
    stage: Stage,
    profiles: Vec<LlmProfile>,
    active: String,
    draft: LlmProfile,
    pub(crate) notice: Option<String>,
}

impl LlmManager {
    pub(crate) fn new(store: &LlmStore<'_>) -> Result<Self, LlmError> {
        let mut manager = Self {
            stage: Stage::Home,
            profiles: Vec::new(),
            active: String::new(),
            draft: Self::empty_draft(),
            notice: None,
        };
        manager.home(store)?;
        Ok(manager)
    }

    fn empty_draft() -> LlmProfile {
        LlmProfile {
            id: "my-llm".into(),
            provider: Provider::Local,
            base_url: "http://localhost:8000/v1".into(),
            model: String::new(),
            context_tokens: 32768,
            tools: false,
            json_schema: false,
        }
    }

    pub(crate) fn page(&self) -> LlmPage {
        let list = |title: String, items: &[&str]| LlmPage::List {
            title,
            items: items.iter().map(|s| (*s).into()).collect(),
        };
        match self.stage {
            Stage::Home => LlmPage::List {
                title: "Подключения LLM · * общая модель для всех чатов".into(),
                items: std::iter::once("Добавить HTTP-подключение".into())
                    .chain(self.profiles.iter().map(|p| {
                        format!(
                            "{} {}",
                            if p.id == self.active { "*" } else { " " },
                            p.label()
                        )
                    }))
                    .chain(std::iter::once("Вернуться в чат".into()))
                    .collect(),
            },
            Stage::Card(index) => {
                let profile = &self.profiles[index];
                let mut items = vec!["Использовать для всех чатов".into()];
                if profile.provider == Provider::Local {
                    items.push("Удалить подключение".into());
                }
                items.push("Назад".into());
                LlmPage::List {
                    title: Self::details(profile),
                    items,
                }
            }
            Stage::Field(field) => {
                let (title, value, hint) = match field {
                    Field::Id => (
                        "Имя подключения (ID)",
                        self.draft.id.clone(),
                        "Латинские буквы, цифры, _ - . :; имя должно быть уникальным",
                    ),
                    Field::Url => (
                        "Базовый URL API",
                        self.draft.base_url.clone(),
                        "Например http://localhost:8000/v1; /chat/completions добавится автоматически",
                    ),
                    Field::Model => (
                        "Имя модели на сервере",
                        self.draft.model.clone(),
                        "Точное значение поля model вашего HTTP API",
                    ),
                    Field::Context => (
                        "Размер контекста в токенах",
                        self.draft.context_tokens.to_string(),
                        "Лимит вашего сервера, не меньше 512",
                    ),
                };
                LlmPage::Text {
                    title: title.into(),
                    value,
                    hint: hint.into(),
                }
            }
            Stage::Tools => list(
                "Сервер поддерживает tools (вызов инструментов и агентов)?".into(),
                &["Нет", "Да"],
            ),
            Stage::Schema => list(
                "Сервер поддерживает Structured Output через JSON Schema?".into(),
                &["Нет", "Да"],
            ),
            Stage::Confirm => list(
                format!("{}\nСохранить подключение?", Self::details(&self.draft)),
                &[
                    "Сохранить и использовать для всех чатов",
                    "Только сохранить",
                    "Отмена",
                ],
            ),
            Stage::Delete(index) => list(
                format!("Удалить подключение {}?", self.profiles[index].id),
                &["Отмена", "Удалить"],
            ),
        }
    }

    fn details(profile: &LlmProfile) -> String {
        let yes = |value| if value { "да" } else { "нет" };
        format!(
            "{}\nURL: {}\nМодель: {} · контекст: {}\nTools: {} · JSON Schema: {}",
            profile.id,
            profile.base_url,
            profile.model,
            profile.context_tokens,
            yes(profile.tools),
            yes(profile.json_schema)
        )
    }

    /// Returns true only when the menu should close.
    pub(crate) fn select(&mut self, index: usize, store: &LlmStore<'_>) -> Result<bool, LlmError> {
        let LlmPage::List { items, .. } = self.page() else {
            return Ok(false);
        };
        if index >= items.len() {
            return Ok(false);
        }
        self.notice = None;
        match self.stage {
            Stage::Home => {
                if index == 0 {
                    self.draft = Self::empty_draft();
                    self.stage = Stage::Field(Field::Id);
                } else if index == self.profiles.len() + 1 {
                    return Ok(true);
                } else {
                    self.stage = Stage::Card(index - 1);
                }
            }
            Stage::Card(profile_index) => {
                let profile = &self.profiles[profile_index];
                if index == 0 {
                    store.set_default(&profile.id)?;
                    self.notice = Some(format!("Общая LLM: {}", profile.id));
                    self.home(store)?;
                } else if index == 1 && profile.provider == Provider::Local {
                    self.stage = Stage::Delete(profile_index);
                } else {
                    self.home(store)?;
                }
            }
            Stage::Tools => {
                self.draft.tools = index == 1;
                self.stage = Stage::Schema;
            }
            Stage::Schema => {
                self.draft.json_schema = index == 1;
                self.stage = Stage::Confirm;
            }
            Stage::Confirm => {
                if index < 2 {
                    store.add(self.draft.clone())?;
                    if index == 0 {
                        store.set_default(&self.draft.id)?;
                    }
                    self.notice = Some(format!(
                        "Подключение {} сохранено{}",
                        self.draft.id,
                        if index == 0 {
                            " и выбрано для всех чатов"
                        } else {
                            ""
                        }
                    ));
                }
                self.home(store)?;
            }
            Stage::Delete(profile_index) => {
                if index == 1 {
                    store.remove(&self.profiles[profile_index].id)?;
                    self.notice = Some("Подключение удалено".into());
                }
                self.home(store)?;
            }
            Stage::Field(_) => {}
        }
        Ok(false)
    }

    pub(crate) fn submit(&mut self, value: String, store: &LlmStore<'_>) -> Result<(), LlmError> {
        let Stage::Field(field) = self.stage else {
            return Ok(());
        };
        let mut draft = self.draft.clone();
        let value = value.trim();
        // Empty input accepts a displayed default, but the model must be supplied.
        match field {
            Field::Id => {
                if !value.is_empty() {
                    draft.id = value.into();
                }
                if draft.id.starts_with("neuraldeep:")
                    || store.list()?.iter().any(|p| p.id == draft.id)
                {
                    return Err(LlmError::Validation(
                        "Такое имя уже существует или зарезервировано".into(),
                    ));
                }
            }
            Field::Url => {
                if !value.is_empty() {
                    draft.base_url = value.trim_end_matches('/').into();
                }
            }
            Field::Model => draft.model = value.into(),
            Field::Context => {
                if !value.is_empty() {
                    draft.context_tokens = value.parse().map_err(|_| {
                        LlmError::Validation("Введите целое число токенов не меньше 512".into())
                    })?;
                }
            }
        }
        let mut validation = draft.clone();
        if !matches!(field, Field::Model) && validation.model.is_empty() {
            validation.model = "pending".into();
        }
        validation.validate()?;
        self.draft = draft;
        self.notice = None;
        self.stage = match field {
            Field::Id => Stage::Field(Field::Url),
            Field::Url => Stage::Field(Field::Model),
            Field::Model => Stage::Field(Field::Context),
            Field::Context => Stage::Tools,
        };
        Ok(())
    }

    pub(crate) fn cancel(&mut self, store: &LlmStore<'_>) -> Result<bool, LlmError> {
        if matches!(self.stage, Stage::Home) {
            return Ok(true);
        }
        self.notice = None;
        self.home(store)?;
        Ok(false)
    }

    fn home(&mut self, store: &LlmStore<'_>) -> Result<(), LlmError> {
        self.profiles = store.list()?;
        self.profiles
            .sort_by_key(|p| p.provider == Provider::NeuralDeep);
        self.active = store.active()?.id;
        self.stage = Stage::Home;
        Ok(())
    }
}

pub(crate) fn run<I: LineInput, W: Write>(
    store: &LlmStore<'_>,
    input: &mut I,
    output: &mut W,
) -> io::Result<()> {
    let mut manager = LlmManager::new(store).map_err(io::Error::other)?;
    loop {
        if let Some(notice) = manager.notice.take() {
            writeln!(output, "{}", sanitize_terminal_text(&notice))?;
        }
        let result = match manager.page() {
            LlmPage::List { title, items } => {
                let items = items
                    .iter()
                    .map(|item| sanitize_terminal_text(item))
                    .collect::<Vec<_>>();
                match input.select(&sanitize_terminal_text(&title), &items)? {
                    Some(index) => manager.select(index, store),
                    None => manager.cancel(store),
                }
            }
            LlmPage::Text { title, value, hint } => {
                writeln!(
                    output,
                    "{}\nТекущее значение: {} · Enter: оставить",
                    sanitize_terminal_text(&hint),
                    sanitize_terminal_text(&value)
                )?;
                match input.read_line(&format!("{title} (esc: отмена): "))? {
                    None => return Ok(()),
                    Some(value) if value.eq_ignore_ascii_case("esc") => manager.cancel(store),
                    Some(value) => manager.submit(value, store).map(|()| false),
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
    use rusqlite::Connection;

    fn connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE llm_profiles(id TEXT PRIMARY KEY,profile_json TEXT NOT NULL); CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE agents(settings_json TEXT);").unwrap();
        connection
    }

    fn complete_form(manager: &mut LlmManager, store: &LlmStore<'_>, id: &str) {
        manager.select(0, store).unwrap();
        for value in [id, "http://127.0.0.1:1/v1/", "my-model", "8192"] {
            manager.submit(value.into(), store).unwrap();
        }
        manager.select(1, store).unwrap(); // tools
        manager.select(0, store).unwrap(); // JSON Schema
    }

    #[test]
    fn saves_only_after_confirmation_and_persists_global_choice() {
        let connection = connection();
        let store = LlmStore::new(&connection);
        let mut manager = LlmManager::new(&store).unwrap();
        complete_form(&mut manager, &store, "mine");
        assert!(store.get("mine").is_err());
        manager.select(0, &store).unwrap();
        let saved = LlmStore::new(&connection).active().unwrap();
        assert_eq!(saved.id, "mine");
        assert_eq!(saved.model, "my-model");
        assert_eq!(saved.context_tokens, 8192);
        assert_eq!(saved.base_url, "http://127.0.0.1:1/v1");
        assert!(saved.tools);
        assert!(!saved.json_schema);
        assert!(
            matches!(manager.page(), LlmPage::List { items, .. } if items[1].starts_with("* mine"))
        );
    }

    #[test]
    fn save_without_selecting_and_cancel_do_not_change_default() {
        let connection = connection();
        let store = LlmStore::new(&connection);
        let original = store.active().unwrap();
        let mut manager = LlmManager::new(&store).unwrap();
        complete_form(&mut manager, &store, "mine");
        manager.select(1, &store).unwrap();
        assert_eq!(store.active().unwrap(), original);
        complete_form(&mut manager, &store, "cancelled");
        manager.cancel(&store).unwrap();
        assert!(store.get("cancelled").is_err());
        assert_eq!(store.active().unwrap(), original);
    }

    #[test]
    fn invalid_inputs_keep_the_current_step_and_duplicate_names_are_rejected() {
        let connection = connection();
        let store = LlmStore::new(&connection);
        let mut manager = LlmManager::new(&store).unwrap();
        manager.select(0, &store).unwrap();
        assert!(manager.submit("bad name".into(), &store).is_err());
        assert!(matches!(manager.stage, Stage::Field(Field::Id)));
        assert!(
            manager
                .submit("neuraldeep:reserved".into(), &store)
                .is_err()
        );
        manager.submit("mine".into(), &store).unwrap();
        for value in [
            "file:///tmp/model",
            "http://secret@localhost/v1",
            "http://localhost/v1?key=secret",
        ] {
            assert!(manager.submit(value.into(), &store).is_err());
            assert!(matches!(manager.stage, Stage::Field(Field::Url)));
        }
        manager.submit(String::new(), &store).unwrap();
        assert!(manager.submit(String::new(), &store).is_err());
        manager.submit("my-model".into(), &store).unwrap();
        for value in ["text", "511", "-1"] {
            assert!(manager.submit(value.into(), &store).is_err());
            assert!(matches!(manager.stage, Stage::Field(Field::Context)));
        }
        manager.cancel(&store).unwrap();
        complete_form(&mut manager, &store, "mine");
        manager.select(1, &store).unwrap();
        manager.select(0, &store).unwrap();
        assert!(manager.submit("mine".into(), &store).is_err());
    }

    #[test]
    fn menu_selects_profiles_and_protects_active_or_agent_profiles_from_deletion() {
        let connection = connection();
        let store = LlmStore::new(&connection);
        let mut manager = LlmManager::new(&store).unwrap();
        complete_form(&mut manager, &store, "mine");
        manager.select(1, &store).unwrap();
        manager.select(1, &store).unwrap(); // custom profile card
        manager.select(0, &store).unwrap(); // choose globally
        assert_eq!(store.active().unwrap().id, "mine");
        manager.select(1, &store).unwrap();
        manager.select(1, &store).unwrap(); // delete confirmation
        assert!(manager.select(1, &store).is_err());
        let builtin = LlmProfile::neuraldeep(crate::config::DEFAULT_MODEL);
        store.set_default(&builtin.id).unwrap();
        let mut settings = crate::settings::Settings::default();
        settings.apply_profile(store.get("mine").unwrap());
        connection
            .execute(
                "INSERT INTO agents(settings_json) VALUES (?1)",
                [serde_json::to_string(&settings).unwrap()],
            )
            .unwrap();
        assert!(manager.select(1, &store).is_err());
        connection.execute("DELETE FROM agents", []).unwrap();
        manager.select(1, &store).unwrap();
        assert!(store.get("mine").is_err());
        assert_eq!(store.active().unwrap(), builtin);
    }
}
