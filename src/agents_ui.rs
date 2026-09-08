use std::io::{self, Write};

use uuid::Uuid;

use crate::agent::{Agent, SystemPromptRequest};

use crate::agent_catalog::{
    AgentDefinition, AgentStore, CatalogError, validate_handle, validate_text,
};
use crate::input::LineInput;
use crate::settings::Settings;
use crate::ui::sanitize_terminal_text;

const FIELDS: &[&str] = &[
    "Имя",
    "Handle (без @)",
    "Краткое описание",
    "System prompt",
    "Модель",
    "Температура",
    "Structured Output",
    "Max tokens",
    "Условие завершения",
];

pub(crate) enum AgentPage {
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
    },
}

enum Stage {
    Home,
    View(Uuid),
    Card(Uuid),
    Delete(Uuid),
    Fields,
    Field(usize),
    ConditionValue(bool),
    Confirm,
}

pub(crate) struct AgentManager {
    stage: Stage,
    agents: Vec<AgentDefinition>,
    draft: AgentDefinition,
    creating: bool,
    system_prompt_preview: Option<String>,
    pub(crate) notice: Option<String>,
}

impl AgentManager {
    pub(crate) fn new(store: &AgentStore<'_>) -> Result<Self, CatalogError> {
        Ok(Self {
            stage: Stage::Home,
            agents: store.list()?,
            draft: AgentDefinition::draft(),
            creating: false,
            system_prompt_preview: None,
            notice: None,
        })
    }

    pub(crate) fn page(&self) -> AgentPage {
        match self.stage {
            Stage::Home => {
                let mut items = vec!["Создать агента".into()];
                items.extend(self.agents.iter().map(AgentDefinition::label));
                items.push("Вернуться".into());
                AgentPage::List {
                    title: format!("Глобальные агенты ({}/50)", self.agents.len()),
                    items,
                }
            }
            Stage::View(_) => AgentPage::Read {
                title: self.draft.label(),
                content: format!(
                    "UUID: {}\nВызов: @{} <задача>\n\n{}\n\nSystem prompt:\n{}",
                    self.draft.id,
                    self.draft.handle,
                    self.draft.settings.menu_items().join("\n"),
                    self.draft.settings.system_prompt().unwrap_or_default()
                ),
            },
            Stage::Card(_) => AgentPage::List {
                title: self.draft.label(),
                items: vec!["Изменить".into(), "Удалить".into(), "Вернуться".into()],
            },
            Stage::Delete(_) => AgentPage::List {
                title: format!(
                    "Удалить {} (@{}) окончательно?",
                    self.draft.name, self.draft.handle
                ),
                items: vec!["Отмена".into(), "Удалить".into()],
            },
            Stage::Fields => {
                let mut items = FIELDS
                    .iter()
                    .map(|field| (*field).to_owned())
                    .collect::<Vec<_>>();
                items.extend(["Сохранить изменения".into(), "Отменить изменения".into()]);
                AgentPage::List {
                    title: format!("Редактирование @{}", self.draft.handle),
                    items,
                }
            }
            Stage::Field(4) => AgentPage::List {
                title: "Модель агента".into(),
                items: Settings::model_items(),
            },
            Stage::Field(6) => AgentPage::List {
                title: "Structured Output агента".into(),
                items: vec!["Выключен".into(), "Включён".into()],
            },
            Stage::Field(8) => AgentPage::List {
                title: "Условие завершения".into(),
                items: vec![
                    "Без условия".into(),
                    "Stop sequence".into(),
                    "Инструкция".into(),
                ],
            },
            Stage::Field(field) => AgentPage::Text {
                title: FIELDS[field].into(),
                value: if self.creating && field != 3 {
                    String::new()
                } else {
                    self.field_value(field)
                },
                multiline: field == 3,
            },
            Stage::ConditionValue(instruction) => AgentPage::Text {
                title: if instruction {
                    "Инструкция завершения"
                } else {
                    "Stop sequence"
                }
                .into(),
                value: if instruction {
                    self.draft.settings.completion_instruction()
                } else {
                    self.draft.settings.stop_sequence()
                }
                .unwrap_or_default()
                .into(),
                multiline: instruction,
            },
            Stage::Confirm => AgentPage::List {
                title: format!("Создать {} (@{})?", self.draft.name, self.draft.handle),
                items: vec!["Создать".into(), "Отмена".into()],
            },
        }
    }

    pub(crate) fn cancel(&mut self, store: &AgentStore<'_>) -> Result<bool, CatalogError> {
        self.system_prompt_preview = None;
        if matches!(self.stage, Stage::Home) {
            return Ok(true);
        }
        if matches!(self.stage, Stage::Field(_) | Stage::ConditionValue(_)) && !self.creating {
            self.stage = Stage::Fields;
        } else {
            self.home(store)?;
        }
        Ok(false)
    }

    pub(crate) fn is_system_prompt(&self) -> bool {
        matches!(self.stage, Stage::Field(3))
    }

    pub(crate) fn system_prompt_request(
        &self,
        request: String,
        fallback_model: &str,
    ) -> SystemPromptRequest {
        SystemPromptRequest {
            name: self.draft.name.clone(),
            handle: self.draft.handle.clone(),
            description: self.draft.description.clone(),
            instructions: self
                .draft
                .settings
                .system_prompt()
                .unwrap_or_default()
                .to_owned(),
            request,
            model: if self.creating {
                fallback_model
            } else {
                self.draft.settings.model()
            }
            .to_owned(),
        }
    }

    pub(crate) fn preview_system_prompt(&mut self, prompt: String) {
        if self.is_system_prompt() {
            self.system_prompt_preview = Some(prompt);
            self.notice = Some(
                "System prompt получен. Отредактируйте и подтвердите: Ctrl+Enter/F4 в TUI или /accept в REPL"
                    .into(),
            );
        }
    }

    pub(crate) fn select(
        &mut self,
        index: usize,
        store: &AgentStore<'_>,
    ) -> Result<bool, CatalogError> {
        self.notice = None;
        match self.stage {
            Stage::Home => {
                if index == 0 {
                    if self.agents.len() >= 50 {
                        return Err(CatalogError::Validation(
                            "Достигнут лимит 50 агентов".into(),
                        ));
                    }
                    self.draft = AgentDefinition::draft();
                    self.creating = true;
                    self.stage = Stage::Field(0);
                } else if let Some(agent) = self.agents.get(index - 1) {
                    self.draft = store.get(agent.id)?;
                    self.stage = Stage::View(agent.id);
                } else {
                    return Ok(true);
                }
            }
            Stage::View(id) => self.stage = Stage::Card(id),
            Stage::Card(id) => match index {
                0 => {
                    self.draft = store.get(id)?;
                    self.creating = false;
                    self.stage = Stage::Fields;
                }
                1 => self.stage = Stage::Delete(id),
                _ => self.home(store)?,
            },
            Stage::Delete(id) => {
                if index == 1 {
                    store.delete(id)?;
                }
                self.home(store)?;
                if index == 1 {
                    self.notice = Some("Агент удалён окончательно".into());
                }
            }
            Stage::Fields => {
                if index < FIELDS.len() {
                    self.stage = Stage::Field(index);
                } else if index == FIELDS.len() {
                    store.save(&self.draft, false)?;
                    self.home(store)?;
                    self.notice = Some("Агент сохранён".into());
                } else {
                    self.home(store)?;
                }
            }
            Stage::Field(4) => {
                self.draft.settings.select_model(index);
                self.next(4);
            }
            Stage::Field(6) => {
                self.notice = self.draft.settings.set_response_format(index == 1);
                self.next(6);
            }
            Stage::Field(8) => match index {
                0 => {
                    self.draft.settings.clear_completion_condition();
                    self.next(8);
                }
                1 | 2 => self.stage = Stage::ConditionValue(index == 2),
                _ => {}
            },
            Stage::Confirm => {
                if index == 0 {
                    store.save(&self.draft, true)?;
                }
                self.home(store)?;
                if index == 0 {
                    self.notice = Some("Агент создан".into());
                }
            }
            _ => {}
        }
        Ok(false)
    }

    pub(crate) fn submit(
        &mut self,
        value: String,
        store: &AgentStore<'_>,
    ) -> Result<(), CatalogError> {
        self.notice = None;
        match self.stage {
            Stage::Field(field) => {
                match field {
                    0 => {
                        validate_text("Имя", &value, 80)?;
                        self.draft.name = value.trim().into();
                    }
                    1 => {
                        let handle = value.trim().to_ascii_lowercase();
                        validate_handle(&handle)?;
                        if store
                            .list()?
                            .iter()
                            .any(|a| a.handle == handle && a.id != self.draft.id)
                        {
                            return Err(CatalogError::Validation(format!(
                                "Handle @{handle} уже занят"
                            )));
                        }
                        self.draft.handle = handle;
                    }
                    2 => {
                        validate_text("Описание", &value, 240)?;
                        self.draft.description =
                            value.split_whitespace().collect::<Vec<_>>().join(" ");
                    }
                    3 => {
                        validate_text("System prompt", &value, 8000)?;
                        self.draft
                            .settings
                            .set_system_prompt(value)
                            .map_err(CatalogError::Validation)?;
                    }
                    5 => self
                        .draft
                        .settings
                        .set_temperature(value.trim())
                        .map_err(CatalogError::Validation)?,
                    7 => {
                        self.notice = self
                            .draft
                            .settings
                            .set_max_tokens(value.trim())
                            .map_err(CatalogError::Validation)?;
                    }
                    _ => {}
                }
                self.next(field);
            }
            Stage::ConditionValue(instruction) => {
                validate_text("Условие завершения", &value, 8000)?;
                if instruction {
                    self.draft.settings.set_completion_instruction(value)
                } else {
                    self.draft.settings.set_stop_sequence(value)
                }
                .map_err(CatalogError::Validation)?;
                self.next(8);
            }
            _ => {}
        }
        Ok(())
    }

    fn next(&mut self, field: usize) {
        self.system_prompt_preview = None;
        self.stage = if !self.creating {
            Stage::Fields
        } else if field + 1 == FIELDS.len() {
            Stage::Confirm
        } else {
            Stage::Field(field + 1)
        };
    }

    fn field_value(&self, field: usize) -> String {
        match field {
            0 => self.draft.name.clone(),
            1 => self.draft.handle.clone(),
            2 => self.draft.description.clone(),
            3 => self
                .system_prompt_preview
                .as_deref()
                .or(self.draft.settings.system_prompt())
                .unwrap_or_default()
                .to_owned(),
            5 => self.draft.settings.temperature().to_string(),
            7 => self.draft.settings.max_tokens().to_string(),
            _ => String::new(),
        }
    }

    fn home(&mut self, store: &AgentStore<'_>) -> Result<(), CatalogError> {
        self.agents = store.list()?;
        self.stage = Stage::Home;
        self.creating = false;
        self.system_prompt_preview = None;
        Ok(())
    }
}

pub(crate) async fn run<I: LineInput, W: Write>(
    agent: &Agent,
    model: &str,
    store: &AgentStore<'_>,
    input: &mut I,
    output: &mut W,
) -> io::Result<()> {
    let mut manager = match AgentManager::new(store) {
        Ok(manager) => manager,
        Err(error) => {
            return writeln!(output, "Каталог агентов: {error}");
        }
    };
    loop {
        if let Some(notice) = manager.notice.take() {
            writeln!(output, "{}", sanitize_terminal_text(&notice))?;
        }
        let result = match manager.page() {
            AgentPage::List { title, items } => {
                let items = items
                    .iter()
                    .map(|item| sanitize_terminal_text(item))
                    .collect::<Vec<_>>();
                match input.select(&sanitize_terminal_text(&title), &items)? {
                    Some(index) => manager.select(index, store),
                    None => manager.cancel(store),
                }
            }
            AgentPage::Text { title, value, .. } => {
                if !value.is_empty() {
                    writeln!(
                        output,
                        "Текущее значение: {}",
                        sanitize_terminal_text(&value)
                    )?;
                }
                if manager.is_system_prompt() {
                    writeln!(
                        output,
                        "/generate <пожелания к агенту> — создать system prompt через LLM; /accept — принять полученный текст"
                    )?;
                }
                let Some(value) = input.read_line(&format!("{title} (esc: отмена): "))?
                else {
                    return Ok(());
                };
                if value == "esc" {
                    manager.cancel(store)
                } else if manager.is_system_prompt()
                    && (value == "/generate" || value.starts_with("/generate "))
                {
                    let query = value.strip_prefix("/generate").unwrap_or_default().trim();
                    let query = if query.is_empty() {
                        manager.field_value(3)
                    } else {
                        query.to_owned()
                    };
                    writeln!(output, "Запрашиваю system prompt у LLM…")?;
                    output.flush()?;
                    match agent
                        .generate_system_prompt(manager.system_prompt_request(query, model))
                        .await
                    {
                        Ok(prompt) => manager.preview_system_prompt(prompt),
                        Err(error) => {
                            manager.notice = Some(format!("System prompt не изменён: {error}"))
                        }
                    }
                    Ok(false)
                } else if manager.is_system_prompt() && value == "/accept" {
                    manager
                        .submit(manager.field_value(3), store)
                        .map(|()| false)
                } else {
                    manager.submit(value, store).map(|()| false)
                }
            }
            AgentPage::Read { title, content } => {
                writeln!(
                    output,
                    "{}\n{}",
                    sanitize_terminal_text(&title),
                    sanitize_terminal_text(&content)
                )?;
                let Some(_) = input.read_line("Enter: действия, Ctrl+D: выход: ")?
                else {
                    return Ok(());
                };
                manager.select(0, store)
            }
        };
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => writeln!(
                output,
                "Ошибка: {}",
                sanitize_terminal_text(&error.to_string())
            )?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatStore;
    use crate::input::BufferedInput;
    use std::io::Cursor;

    #[tokio::test]
    async fn repl_can_generate_review_and_accept_system_prompt() {
        let server = crate::test_http::MockServer::new(|_| {
            (200, serde_json::json!({"choices":[{"message":{"content":"Исправляет ошибки и улучшает стиль текста."},"finish_reason":"stop"}]}).to_string())
        });
        let agent = Agent::new(
            crate::api::NeuralDeepClient::new("test-key".into(), server.url.clone())
                .expect("client"),
        );
        let path = std::env::temp_dir().join(format!("agi-generate-{}", Uuid::new_v4()));
        let db = ChatStore::for_tests(path.clone()).expect("db");
        let mut input = BufferedInput::new(Cursor::new(
            "1\nРедактор\neditor\nПроверяет текст\n/generate Помощник по текстам\n/accept\n12\n0.5\n1\n500\n1\n1\nesc\n",
        ));
        let mut output = Vec::new();
        run(&agent, "gpt-oss-20b", &db.agents(), &mut input, &mut output)
            .await
            .expect("menu");
        let agents = db.agents().list().expect("agents");
        assert_eq!(
            agents[0].settings.system_prompt(),
            Some("Исправляет ошибки и улучшает стиль текста.")
        );
        assert_eq!(agents[0].description, "Проверяет текст");
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.requests()[0]["model"], "gpt-oss-20b");
        assert!(
            String::from_utf8(output)
                .expect("output")
                .contains("System prompt получен")
        );
        drop(db);
        std::fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn generated_system_prompt_is_only_a_preview_until_confirmed() {
        let path = std::env::temp_dir().join(format!("agi-preview-{}", Uuid::new_v4()));
        let db = ChatStore::for_tests(path.clone()).expect("db");
        let mut manager = AgentManager::new(&db.agents()).expect("manager");
        manager.stage = Stage::Field(3);
        manager.draft.description = "Старое описание".into();
        manager
            .draft
            .settings
            .set_system_prompt("Старый prompt".into())
            .expect("prompt");
        manager.preview_system_prompt("Предложенный prompt".into());
        assert_eq!(manager.field_value(3), "Предложенный prompt");
        assert_eq!(
            manager.draft.settings.system_prompt(),
            Some("Старый prompt")
        );
        manager.cancel(&db.agents()).expect("cancel");
        assert_eq!(
            manager.draft.settings.system_prompt(),
            Some("Старый prompt")
        );
        manager.stage = Stage::Field(3);
        manager.preview_system_prompt("Новый prompt".into());
        manager
            .submit(manager.field_value(3), &db.agents())
            .expect("accept");
        assert_eq!(manager.draft.settings.system_prompt(), Some("Новый prompt"));
        assert_eq!(manager.draft.description, "Старое описание");
        drop(db);
        std::fs::remove_dir_all(path).expect("cleanup");
    }

    #[test]
    fn wizard_requires_all_fields_and_commits_only_after_confirmation() {
        let path = std::env::temp_dir().join(format!("agi-wizard-{}", Uuid::new_v4()));
        let db = ChatStore::for_tests(path.clone()).expect("db");
        let store = db.agents();
        let mut manager = AgentManager::new(&store).expect("manager");
        manager.select(0, &store).expect("create");
        for value in ["Редактор", "EDITOR", "Проверяет текст", "Исправь ошибки"]
        {
            manager.submit(value.into(), &store).expect("field");
            assert!(store.list().expect("list").is_empty());
        }
        manager.select(11, &store).expect("model");
        assert!(manager.submit("NaN".into(), &store).is_err());
        manager.submit("0.6".into(), &store).expect("temperature");
        manager.select(1, &store).expect("structured");
        assert!(manager.submit("12".into(), &store).is_err());
        manager.submit("600".into(), &store).expect("tokens");
        manager.select(2, &store).expect("completion type");
        manager
            .submit("Сделай вывод".into(), &store)
            .expect("completion");
        assert!(store.list().expect("list").is_empty());
        manager.select(0, &store).expect("confirm");
        let agents = store.list().expect("list");
        let saved = &agents[0];
        assert_eq!(saved.handle, "editor");
        assert_eq!(saved.settings.temperature(), 0.6);
        assert!(saved.settings.response_format_enabled());
        assert_eq!(saved.settings.max_tokens(), 600);
        manager.select(1, &store).expect("view");
        manager.select(0, &store).expect("actions");
        manager.select(0, &store).expect("edit");
        manager.select(0, &store).expect("name");
        manager.submit("Новое имя".into(), &store).expect("draft");
        manager.cancel(&store).expect("discard draft");
        assert_eq!(store.get(saved.id).expect("agent").name, "Редактор");
        manager.select(1, &store).expect("view");
        manager.select(0, &store).expect("actions");
        manager.select(1, &store).expect("delete dialog");
        manager.select(0, &store).expect("cancel delete");
        assert_eq!(store.list().expect("list").len(), 1);
        manager.select(1, &store).expect("view");
        manager.select(0, &store).expect("actions");
        manager.select(1, &store).expect("delete dialog");
        manager.select(1, &store).expect("confirm delete");
        assert!(store.list().expect("list").is_empty());
        drop(db);
        std::fs::remove_dir_all(path).expect("cleanup");
    }

    #[tokio::test]
    async fn repl_wizard_creates_views_and_cancels_without_partial_records() {
        let path = std::env::temp_dir().join(format!("agi-menu-{}", Uuid::new_v4()));
        let db = ChatStore::for_tests(path.clone()).expect("db");
        let mut input = BufferedInput::new(Cursor::new(
            "1\nРедактор\neditor\nПроверяет текст\nИсправляй ошибки\n12\n0.5\n1\n500\n1\n1\n2\n\n3\n1\nЧерновик\nesc\nesc\n",
        ));
        let mut output = Vec::new();
        let agent = Agent::new(
            crate::api::NeuralDeepClient::new("test-key".into(), "http://127.0.0.1:1".into())
                .expect("client"),
        );
        run(
            &agent,
            crate::config::DEFAULT_MODEL,
            &db.agents(),
            &mut input,
            &mut output,
        )
        .await
        .expect("menu");
        assert_eq!(db.agents().list().expect("agents").len(), 1);
        let output = String::from_utf8(output).expect("UTF-8");
        assert!(output.contains("Агент создан"));
        assert!(output.contains("Вызов: @editor"));
        drop(db);
        std::fs::remove_dir_all(path).expect("cleanup");
    }
}
