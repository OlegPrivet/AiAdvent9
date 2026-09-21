use std::io::{self, Write};
use std::path::PathBuf;

use uuid::Uuid;

use crate::input::LineInput;
use crate::mcp::{
    McpError, McpServerDefinition, McpStore, McpTool, McpTransport, demo_definition,
    inspect_server, parse_args,
};
use crate::ui::sanitize_terminal_text;

pub(crate) enum McpPage {
    List {
        title: String,
        items: Vec<String>,
    },
    Text {
        title: String,
        value: String,
        hint: String,
    },
    Read {
        title: String,
        content: String,
    },
}

#[derive(Clone, Copy)]
enum Field {
    Name,
    Command,
    Arguments,
    WorkingDirectory,
    Url,
    TokenEnvironment,
}

enum Stage {
    Home,
    Card(Uuid),
    Field(Field),
    Confirm,
    Delete(Uuid),
    Read(Uuid),
}

pub(crate) enum McpAction {
    None,
    Close,
    Inspect(McpServerDefinition),
}

pub(crate) struct McpManager {
    stage: Stage,
    servers: Vec<McpServerDefinition>,
    draft: Option<McpServerDefinition>,
    creating: bool,
    read_content: String,
    pub(crate) notice: Option<String>,
}

impl McpManager {
    pub(crate) fn new(store: &McpStore<'_>) -> Result<Self, McpError> {
        Ok(Self {
            stage: Stage::Home,
            servers: store.list()?,
            draft: None,
            creating: false,
            read_content: String::new(),
            notice: None,
        })
    }

    pub(crate) fn page(&self) -> McpPage {
        match self.stage {
            Stage::Home => {
                let mut items = vec![
                    "Добавить stdio-сервер".into(),
                    "Добавить Streamable HTTP-сервер".into(),
                    "Добавить встроенный демосервер".into(),
                ];
                items.extend(self.servers.iter().map(McpServerDefinition::label));
                items.push("Вернуться".into());
                McpPage::List {
                    title: format!("MCP-серверы ({})", self.servers.len()),
                    items,
                }
            }
            Stage::Card(_) => {
                let draft = self.draft.as_ref().expect("карточка содержит сервер");
                McpPage::List {
                    title: draft.label(),
                    items: vec![
                        "Проверить подключение и показать инструменты".into(),
                        if draft.enabled {
                            "Выключить для AI"
                        } else {
                            "Включить для AI"
                        }
                        .into(),
                        "Изменить настройки".into(),
                        "Удалить".into(),
                        "Вернуться".into(),
                    ],
                }
            }
            Stage::Delete(_) => McpPage::List {
                title: format!(
                    "Удалить MCP-сервер «{}»?",
                    self.draft.as_ref().expect("удаление содержит сервер").name
                ),
                items: vec!["Отмена".into(), "Удалить".into()],
            },
            Stage::Confirm => {
                let action = if self.creating {
                    "Добавить"
                } else {
                    "Сохранить"
                };
                McpPage::List {
                    title: format!(
                        "{action} MCP-сервер «{}»?",
                        self.draft.as_ref().expect("черновик существует").name
                    ),
                    items: vec![
                        format!("{action} и включить для AI"),
                        format!("{action} выключенным"),
                        "Отмена".into(),
                    ],
                }
            }
            Stage::Field(field) => {
                let draft = self.draft.as_ref().expect("черновик существует");
                let (title, value, hint) = match field {
                    Field::Name => ("Имя сервера", draft.name.clone(), "2–40 символов".into()),
                    Field::Command => match &draft.transport {
                        McpTransport::Stdio { command, .. } => (
                            "Исполняемый файл",
                            command.clone(),
                            "Например: npx или /path/to/server".into(),
                        ),
                        _ => unreachable!(),
                    },
                    Field::Arguments => match &draft.transport {
                        McpTransport::Stdio { args, .. } => (
                            "Аргументы команды",
                            serde_json::to_string(args).unwrap_or_else(|_| "[]".into()),
                            "JSON-массив строк, например [\"-y\",\"server\"]".into(),
                        ),
                        _ => unreachable!(),
                    },
                    Field::WorkingDirectory => match &draft.transport {
                        McpTransport::Stdio {
                            working_directory, ..
                        } => (
                            "Рабочий каталог",
                            working_directory.to_string_lossy().into_owned(),
                            "Абсолютный или относительный путь".into(),
                        ),
                        _ => unreachable!(),
                    },
                    Field::Url => match &draft.transport {
                        McpTransport::Http { url, .. } => (
                            "URL MCP endpoint",
                            url.clone(),
                            "http:// или https://".into(),
                        ),
                        _ => unreachable!(),
                    },
                    Field::TokenEnvironment => match &draft.transport {
                        McpTransport::Http {
                            bearer_token_env, ..
                        } => (
                            "Переменная окружения Bearer-токена",
                            bearer_token_env.clone().unwrap_or_default(),
                            "Необязательно; например MY_MCP_TOKEN".into(),
                        ),
                        _ => unreachable!(),
                    },
                };
                McpPage::Text {
                    title: title.into(),
                    value,
                    hint,
                }
            }
            Stage::Read(_) => McpPage::Read {
                title: format!(
                    "Инструменты · {}",
                    self.draft.as_ref().expect("просмотр содержит сервер").name
                ),
                content: self.read_content.clone(),
            },
        }
    }

    pub(crate) fn select(
        &mut self,
        index: usize,
        store: &McpStore<'_>,
    ) -> Result<McpAction, McpError> {
        match self.stage {
            Stage::Home => {
                if index == 0 {
                    self.start_stdio()?;
                } else if index == 1 {
                    self.start_http()?;
                } else if index == 2 {
                    let demo = demo_definition()?;
                    store.save(&demo, true)?;
                    self.notice = Some("Демосервер добавлен и включён".into());
                    self.home(store)?;
                } else if index == self.servers.len() + 3 {
                    return Ok(McpAction::Close);
                } else if let Some(server) = self.servers.get(index.saturating_sub(3)).cloned() {
                    self.draft = Some(server.clone());
                    self.stage = Stage::Card(server.id);
                }
            }
            Stage::Card(id) => match index {
                0 => {
                    self.notice = Some("Устанавливаю MCP-соединение…".into());
                    return Ok(McpAction::Inspect(store.get(id)?));
                }
                1 => {
                    let enabled = !self.draft.as_ref().expect("сервер существует").enabled;
                    store.set_enabled(id, enabled)?;
                    self.notice = Some(
                        if enabled {
                            "MCP-сервер включён для AI"
                        } else {
                            "MCP-сервер выключен для AI"
                        }
                        .into(),
                    );
                    self.draft = Some(store.get(id)?);
                }
                2 => {
                    self.creating = false;
                    self.stage = Stage::Field(Field::Name);
                }
                3 => self.stage = Stage::Delete(id),
                4 => self.home(store)?,
                _ => {}
            },
            Stage::Delete(id) => {
                if index == 1 {
                    store.remove(id)?;
                    self.notice = Some("MCP-сервер удалён".into());
                    self.home(store)?;
                } else {
                    self.stage = Stage::Card(id);
                }
            }
            Stage::Confirm => {
                if index <= 1 {
                    let draft = self.draft.as_mut().expect("черновик существует");
                    draft.enabled = index == 0;
                    store.save(draft, self.creating)?;
                    self.notice = Some(
                        if self.creating {
                            "MCP-сервер добавлен"
                        } else {
                            "Настройки MCP-сервера сохранены"
                        }
                        .into(),
                    );
                    self.home(store)?;
                } else {
                    self.home(store)?;
                }
            }
            Stage::Read(id) => self.stage = Stage::Card(id),
            Stage::Field(_) => {}
        }
        Ok(McpAction::None)
    }

    pub(crate) fn submit(&mut self, value: String, _store: &McpStore<'_>) -> Result<(), McpError> {
        let Stage::Field(field) = self.stage else {
            return Ok(());
        };
        let draft = self.draft.as_mut().expect("черновик существует");
        match field {
            Field::Name => {
                draft.name = value.trim().to_owned();
                self.stage = match draft.transport {
                    McpTransport::Stdio { .. } => Stage::Field(Field::Command),
                    McpTransport::Http { .. } => Stage::Field(Field::Url),
                    #[cfg(test)]
                    McpTransport::InMemoryDemo => Stage::Confirm,
                };
            }
            Field::Command => {
                if let McpTransport::Stdio { command, .. } = &mut draft.transport {
                    *command = value.trim().to_owned();
                }
                self.stage = Stage::Field(Field::Arguments);
            }
            Field::Arguments => {
                if let McpTransport::Stdio { args, .. } = &mut draft.transport {
                    *args = parse_args(&value)?;
                }
                self.stage = Stage::Field(Field::WorkingDirectory);
            }
            Field::WorkingDirectory => {
                if let McpTransport::Stdio {
                    working_directory, ..
                } = &mut draft.transport
                {
                    *working_directory = PathBuf::from(value.trim());
                }
                draft.validate()?;
                self.stage = Stage::Confirm;
            }
            Field::Url => {
                if let McpTransport::Http { url, .. } = &mut draft.transport {
                    *url = value.trim().to_owned();
                }
                self.stage = Stage::Field(Field::TokenEnvironment);
            }
            Field::TokenEnvironment => {
                if let McpTransport::Http {
                    bearer_token_env, ..
                } = &mut draft.transport
                {
                    let value = value.trim();
                    *bearer_token_env = (!value.is_empty()).then(|| value.to_owned());
                }
                draft.validate()?;
                self.stage = Stage::Confirm;
            }
        }
        Ok(())
    }

    pub(crate) fn cancel(&mut self, store: &McpStore<'_>) -> Result<bool, McpError> {
        if matches!(self.stage, Stage::Home) {
            return Ok(true);
        }
        match self.stage {
            Stage::Read(id) | Stage::Delete(id) => self.stage = Stage::Card(id),
            _ => self.home(store)?,
        }
        Ok(false)
    }

    pub(crate) fn inspection_finished(&mut self, result: Result<Vec<McpTool>, McpError>) {
        match result {
            Ok(tools) => {
                let id = self.draft.as_ref().expect("сервер существует").id;
                self.read_content = format_tools(&tools);
                self.notice = Some(format!(
                    "Соединение установлено, инструментов: {}",
                    tools.len()
                ));
                self.stage = Stage::Read(id);
            }
            Err(error) => self.notice = Some(format!("MCP-проверка не выполнена: {error}")),
        }
    }

    fn start_stdio(&mut self) -> Result<(), McpError> {
        let working_directory = std::env::current_dir().map_err(McpError::Spawn)?;
        self.draft = Some(McpServerDefinition {
            id: Uuid::new_v4(),
            name: String::new(),
            transport: McpTransport::Stdio {
                command: String::new(),
                args: Vec::new(),
                working_directory,
            },
            enabled: true,
        });
        self.creating = true;
        self.stage = Stage::Field(Field::Name);
        Ok(())
    }

    fn start_http(&mut self) -> Result<(), McpError> {
        self.draft = Some(McpServerDefinition {
            id: Uuid::new_v4(),
            name: String::new(),
            transport: McpTransport::Http {
                url: String::new(),
                bearer_token_env: None,
            },
            enabled: true,
        });
        self.creating = true;
        self.stage = Stage::Field(Field::Name);
        Ok(())
    }

    fn home(&mut self, store: &McpStore<'_>) -> Result<(), McpError> {
        self.servers = store.list()?;
        self.draft = None;
        self.creating = false;
        self.stage = Stage::Home;
        Ok(())
    }
}

fn format_tools(tools: &[McpTool]) -> String {
    if tools.is_empty() {
        return "Соединение установлено. Сервер не предоставил инструментов.".into();
    }
    tools
        .iter()
        .map(|tool| {
            format!(
                "{}\n{}\nВходная схема:\n{}",
                tool.name,
                if tool.description.is_empty() {
                    "Без описания"
                } else {
                    &tool.description
                },
                serde_json::to_string_pretty(&tool.input_schema).unwrap_or_else(|_| "{}".into())
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) async fn run<I: LineInput, W: Write>(
    store: &McpStore<'_>,
    input: &mut I,
    output: &mut W,
) -> io::Result<()> {
    let mut manager = match McpManager::new(store) {
        Ok(manager) => manager,
        Err(error) => return writeln!(output, "Каталог MCP: {error}"),
    };
    loop {
        if let Some(notice) = manager.notice.take() {
            writeln!(output, "{}", sanitize_terminal_text(&notice))?;
        }
        let action = match manager.page() {
            McpPage::List { title, items } => {
                let items = items
                    .iter()
                    .map(|item| sanitize_terminal_text(item))
                    .collect::<Vec<_>>();
                match input.select(&sanitize_terminal_text(&title), &items)? {
                    Some(index) => manager.select(index, store),
                    None => manager.cancel(store).map(|close| {
                        if close {
                            McpAction::Close
                        } else {
                            McpAction::None
                        }
                    }),
                }
            }
            McpPage::Text { title, value, hint } => {
                if !value.is_empty() {
                    writeln!(
                        output,
                        "Текущее значение: {}",
                        sanitize_terminal_text(&value)
                    )?;
                }
                writeln!(output, "{}", sanitize_terminal_text(&hint))?;
                match input.read_line(&format!("{title} (esc: отмена): "))? {
                    None => return Ok(()),
                    Some(value) if value == "esc" => manager.cancel(store).map(|close| {
                        if close {
                            McpAction::Close
                        } else {
                            McpAction::None
                        }
                    }),
                    Some(value) => manager.submit(value, store).map(|()| McpAction::None),
                }
            }
            McpPage::Read { title, content } => {
                writeln!(
                    output,
                    "{}\n{}",
                    sanitize_terminal_text(&title),
                    sanitize_terminal_text(&content)
                )?;
                if input
                    .read_line("Enter: действия, Ctrl+D: выход: ")?
                    .is_none()
                {
                    return Ok(());
                }
                manager.select(0, store)
            }
        };
        match action {
            Ok(McpAction::Close) => return Ok(()),
            Ok(McpAction::None) => {}
            Ok(McpAction::Inspect(definition)) => {
                writeln!(output, "Устанавливаю MCP-соединение…")?;
                output.flush()?;
                manager.inspection_finished(inspect_server(&definition).await);
            }
            Err(error) => manager.notice = Some(error.to_string()),
        }
    }
}
