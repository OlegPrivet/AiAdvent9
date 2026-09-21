use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool};
use rmcp::service::RunningService;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_SERVERS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        working_directory: PathBuf,
    },
    Http {
        url: String,
        bearer_token_env: Option<String>,
    },
    #[cfg(test)]
    InMemoryDemo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct McpServerDefinition {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) transport: McpTransport,
    pub(crate) enabled: bool,
}

impl McpServerDefinition {
    pub(crate) fn validate(&self) -> Result<(), McpError> {
        let name = self.name.trim();
        if !(2..=40).contains(&name.chars().count()) {
            return Err(McpError::Validation(
                "Имя MCP-сервера: от 2 до 40 символов".into(),
            ));
        }
        if name.chars().any(char::is_control) {
            return Err(McpError::Validation(
                "Имя MCP-сервера содержит управляющие символы".into(),
            ));
        }
        match &self.transport {
            McpTransport::Stdio {
                command,
                working_directory,
                ..
            } => {
                if command.trim().is_empty() {
                    return Err(McpError::Validation("Команда stdio не задана".into()));
                }
                if working_directory.as_os_str().is_empty() {
                    return Err(McpError::Validation("Рабочий каталог не задан".into()));
                }
            }
            McpTransport::Http {
                url,
                bearer_token_env,
            } => {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(McpError::Validation(
                        "URL MCP должен начинаться с http:// или https://".into(),
                    ));
                }
                if bearer_token_env.as_ref().is_some_and(|name| {
                    name.is_empty()
                        || !name
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                }) {
                    return Err(McpError::Validation(
                        "Имя переменной токена: только A-Z, 0-9 и _".into(),
                    ));
                }
            }
            #[cfg(test)]
            McpTransport::InMemoryDemo => {}
        }
        Ok(())
    }

    pub(crate) fn label(&self) -> String {
        format!(
            "{} · {} · {}",
            self.name,
            match self.transport {
                McpTransport::Stdio { .. } => "stdio",
                McpTransport::Http { .. } => "HTTP",
                #[cfg(test)]
                McpTransport::InMemoryDemo => "test",
            },
            if self.enabled {
                "включён"
            } else {
                "выключен"
            }
        )
    }
}

#[derive(Debug, Error)]
pub(crate) enum McpError {
    #[error("{0}")]
    Validation(String),
    #[error("ошибка каталога MCP: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("некорректные настройки MCP: {0}")]
    Json(#[from] serde_json::Error),
    #[error("некорректный UUID MCP-сервера: {0}")]
    Id(#[from] uuid::Error),
    #[error("MCP-сервер не найден")]
    NotFound,
    #[error("MCP-сервер с таким именем уже существует")]
    DuplicateName,
    #[error("допускается не более {MAX_SERVERS} MCP-серверов")]
    Limit,
    #[error("переменная окружения {0} не задана или пуста")]
    MissingToken(String),
    #[error("не удалось запустить MCP-сервер: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("MCP-соединение: {0}")]
    Connection(String),
    #[error("MCP-соединение превысило таймаут")]
    ConnectTimeout,
    #[error("вызов MCP-инструмента превысил таймаут")]
    CallTimeout,
}

pub(crate) struct McpStore<'a> {
    connection: &'a Connection,
}

impl<'a> McpStore<'a> {
    pub(crate) fn new(connection: &'a Connection) -> Self {
        Self { connection }
    }

    pub(crate) fn list(&self) -> Result<Vec<McpServerDefinition>, McpError> {
        let mut statement = self.connection.prepare(
            "SELECT id, name, transport_json, enabled FROM mcp_servers ORDER BY name COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (id, name, transport, enabled) = row?;
            let definition = McpServerDefinition {
                id: Uuid::parse_str(&id)?,
                name,
                transport: serde_json::from_str(&transport)?,
                enabled,
            };
            definition.validate()?;
            Ok(definition)
        })
        .collect()
    }

    pub(crate) fn enabled(&self) -> Result<Vec<McpServerDefinition>, McpError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|server| server.enabled)
            .collect())
    }

    pub(crate) fn get(&self, id: Uuid) -> Result<McpServerDefinition, McpError> {
        self.list()?
            .into_iter()
            .find(|server| server.id == id)
            .ok_or(McpError::NotFound)
    }

    pub(crate) fn save(
        &self,
        definition: &McpServerDefinition,
        creating: bool,
    ) -> Result<(), McpError> {
        definition.validate()?;
        if creating
            && self
                .connection
                .query_row("SELECT COUNT(*) FROM mcp_servers", [], |row| {
                    row.get::<_, i64>(0)
                })?
                >= MAX_SERVERS as i64
        {
            return Err(McpError::Limit);
        }
        let duplicate = self
            .connection
            .query_row(
                "SELECT id FROM mcp_servers WHERE name = ?1 COLLATE NOCASE AND id <> ?2",
                params![definition.name.trim(), definition.id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if duplicate.is_some() {
            return Err(McpError::DuplicateName);
        }
        let transport = serde_json::to_string(&definition.transport)?;
        let changed = self.connection.execute(
            "INSERT INTO mcp_servers(id, name, transport_json, enabled)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name,
                 transport_json=excluded.transport_json, enabled=excluded.enabled",
            params![
                definition.id.to_string(),
                definition.name.trim(),
                transport,
                definition.enabled
            ],
        )?;
        if changed != 1 {
            return Err(McpError::NotFound);
        }
        Ok(())
    }

    pub(crate) fn remove(&self, id: Uuid) -> Result<(), McpError> {
        if self.connection.execute(
            "DELETE FROM mcp_servers WHERE id = ?1",
            params![id.to_string()],
        )? == 0
        {
            return Err(McpError::NotFound);
        }
        Ok(())
    }

    pub(crate) fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<(), McpError> {
        if self.connection.execute(
            "UPDATE mcp_servers SET enabled = ?2 WHERE id = ?1",
            params![id.to_string(), enabled],
        )? == 0
        {
            return Err(McpError::NotFound);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct McpTool {
    pub(crate) function_name: String,
    pub(crate) server_id: Uuid,
    pub(crate) server_name: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

impl McpTool {
    pub(crate) fn openai_definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.function_name,
                "description": format!("MCP {}: {}", self.server_name, self.description),
                "parameters": self.input_schema,
            }
        })
    }
}

pub(crate) struct McpRuntime {
    connections: Vec<ConnectedServer>,
    tools: Vec<McpTool>,
}

struct ConnectedServer {
    definition: McpServerDefinition,
    service: RunningService<RoleClient, ()>,
}

impl McpRuntime {
    pub(crate) async fn connect(
        definitions: &[McpServerDefinition],
    ) -> (Self, Vec<(String, String)>) {
        let mut connections = Vec::new();
        let mut tools = Vec::new();
        let mut errors = Vec::new();
        let mut function_names = HashSet::new();
        for definition in definitions.iter().filter(|server| server.enabled) {
            match connect_server(definition).await {
                Ok(service) => {
                    match timeout(CONNECT_TIMEOUT, service.peer().list_all_tools()).await {
                        Ok(Ok(server_tools)) => {
                            for (tool_index, tool) in server_tools.into_iter().enumerate() {
                                let function_name = unique_function_name(
                                    &definition.name,
                                    &tool.name,
                                    tool_index,
                                    &mut function_names,
                                );
                                tools.push(map_tool(definition, tool, function_name));
                            }
                            connections.push(ConnectedServer {
                                definition: definition.clone(),
                                service,
                            });
                        }
                        Ok(Err(error)) => errors.push((definition.name.clone(), error.to_string())),
                        Err(_) => errors.push((
                            definition.name.clone(),
                            McpError::ConnectTimeout.to_string(),
                        )),
                    }
                }
                Err(error) => errors.push((definition.name.clone(), error.to_string())),
            }
        }
        (Self { connections, tools }, errors)
    }

    pub(crate) fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub(crate) async fn call(
        &self,
        function_name: &str,
        arguments: Value,
    ) -> Result<(McpTool, Value), McpError> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.function_name == function_name)
            .cloned()
            .ok_or_else(|| McpError::Validation("неизвестный MCP-инструмент".into()))?;
        let connection = self
            .connections
            .iter()
            .find(|connection| connection.definition.id == tool.server_id)
            .ok_or(McpError::NotFound)?;
        let arguments = arguments.as_object().cloned().ok_or_else(|| {
            McpError::Validation("аргументы MCP-инструмента должны быть JSON-объектом".into())
        })?;
        let params = CallToolRequestParams::new(tool.name.clone()).with_arguments(arguments);
        let result = timeout(CALL_TIMEOUT, connection.service.call_tool(params))
            .await
            .map_err(|_| McpError::CallTimeout)?
            .map_err(|error| McpError::Connection(error.to_string()))?;
        Ok((tool, tool_result_value(result)))
    }
}

pub(crate) async fn inspect_server(
    definition: &McpServerDefinition,
) -> Result<Vec<McpTool>, McpError> {
    let service = connect_server(definition).await?;
    let tools = timeout(CONNECT_TIMEOUT, service.peer().list_all_tools())
        .await
        .map_err(|_| McpError::ConnectTimeout)?
        .map_err(|error| McpError::Connection(error.to_string()))?;
    let mut names = HashSet::new();
    let result = tools
        .into_iter()
        .enumerate()
        .map(|(index, tool)| {
            let function_name =
                unique_function_name(&definition.name, &tool.name, index, &mut names);
            map_tool(definition, tool, function_name)
        })
        .collect();
    drop(service);
    Ok(result)
}

async fn connect_server(
    definition: &McpServerDefinition,
) -> Result<RunningService<RoleClient, ()>, McpError> {
    definition.validate()?;
    let future = async {
        match &definition.transport {
            McpTransport::Stdio {
                command,
                args,
                working_directory,
            } => {
                let mut command = Command::new(command);
                command.args(args).current_dir(working_directory);
                let (transport, _) = TokioChildProcess::builder(command)
                    .stderr(Stdio::null())
                    .spawn()?;
                ().serve(transport)
                    .await
                    .map_err(|error| McpError::Connection(error.to_string()))
            }
            McpTransport::Http {
                url,
                bearer_token_env,
            } => {
                let mut config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.clone());
                if let Some(name) = bearer_token_env {
                    let token = env::var(name)
                        .ok()
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| McpError::MissingToken(name.clone()))?;
                    config = config.auth_header(token);
                }
                let transport = StreamableHttpClientTransport::from_config(config);
                ().serve(transport)
                    .await
                    .map_err(|error| McpError::Connection(error.to_string()))
            }
            #[cfg(test)]
            McpTransport::InMemoryDemo => {
                let (server_transport, client_transport) = tokio::io::duplex(4096);
                tokio::spawn(async move {
                    let _ = crate::mcp_demo::DemoServer
                        .serve(server_transport)
                        .await
                        .expect("in-memory demo server")
                        .waiting()
                        .await;
                });
                ().serve(client_transport)
                    .await
                    .map_err(|error| McpError::Connection(error.to_string()))
            }
        }
    };
    timeout(CONNECT_TIMEOUT, future)
        .await
        .map_err(|_| McpError::ConnectTimeout)?
}

fn map_tool(definition: &McpServerDefinition, tool: Tool, function_name: String) -> McpTool {
    McpTool {
        function_name,
        server_id: definition.id,
        server_name: definition.name.clone(),
        name: tool.name.into_owned(),
        description: tool.description.map(Cow::into_owned).unwrap_or_default(),
        input_schema: Value::Object((*tool.input_schema).clone()),
    }
}

fn unique_function_name(
    server: &str,
    tool: &str,
    index: usize,
    used: &mut HashSet<String>,
) -> String {
    fn sanitize(value: &str) -> String {
        let mut result = value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if result.starts_with(|c: char| c.is_ascii_digit()) {
            result.insert(0, '_');
        }
        result
    }
    let base = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    let base = base.chars().take(56).collect::<String>();
    let mut candidate = base.clone();
    let mut suffix = index;
    while !used.insert(candidate.clone()) {
        suffix += 1;
        candidate = format!("{base}_{suffix}");
    }
    candidate
}

fn tool_result_value(result: CallToolResult) -> Value {
    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text(text) => json!({"type":"text", "text":text.text}),
            ContentBlock::Image(image) => json!({
                "type":"unsupported_image",
                "mimeType":image.mime_type,
                "message":"Изображение получено, но бинарные MCP-данные не передаются в текстовый контекст"
            }),
            ContentBlock::Audio(audio) => json!({
                "type":"unsupported_audio",
                "mimeType":audio.mime_type,
                "message":"Аудио получено, но бинарные MCP-данные не передаются в текстовый контекст"
            }),
            ContentBlock::Resource(_) => json!({
                "type":"embedded_resource",
                "message":"Встроенный ресурс получен; его бинарное содержимое не передано"
            }),
            ContentBlock::ResourceLink(link) => serde_json::to_value(link)
                .unwrap_or_else(|_| json!({"type":"resource_link"})),
            _ => json!({
                "type":"unsupported_content",
                "message":"MCP вернул неподдерживаемый тип содержимого"
            }),
        })
        .collect::<Vec<_>>();
    json!({
        "ok": !result.is_error.unwrap_or(false),
        "content": content,
        "structuredContent": result.structured_content,
        "isError": result.is_error.unwrap_or(false)
    })
}

pub(crate) fn demo_definition() -> Result<McpServerDefinition, McpError> {
    let executable = env::current_exe().map_err(McpError::Spawn)?;
    let working_directory = env::current_dir().map_err(McpError::Spawn)?;
    Ok(McpServerDefinition {
        id: Uuid::new_v4(),
        name: "demo".into(),
        transport: McpTransport::Stdio {
            command: executable.to_string_lossy().into_owned(),
            args: vec!["--mcp-demo-server".into()],
            working_directory,
        },
        enabled: true,
    })
}

pub(crate) fn parse_args(value: &str) -> Result<Vec<String>, McpError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<String>>(value).map_err(|_| {
        McpError::Validation(
            "Аргументы должны быть JSON-массивом строк, например [\"--flag\",\"value\"]".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE mcp_servers (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL COLLATE NOCASE UNIQUE,
                    transport_json TEXT NOT NULL,
                    enabled INTEGER NOT NULL CHECK(enabled IN (0, 1))
                );",
            )
            .expect("schema");
        connection
    }

    #[test]
    fn parses_stdio_arguments_without_shell() {
        assert_eq!(
            parse_args(r#"["--flag","value with spaces"]"#).unwrap(),
            ["--flag", "value with spaces"]
        );
        assert!(parse_args("--flag value").is_err());
    }

    #[test]
    fn creates_unique_model_function_names() {
        let mut used = HashSet::new();
        let first = unique_function_name("Файлы", "read-file", 0, &mut used);
        let second = unique_function_name("Файлы", "read-file", 0, &mut used);
        assert_ne!(first, second);
        assert!(first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn catalog_crud_keeps_tokens_out_of_storage() {
        let connection = store_connection();
        let store = McpStore::new(&connection);
        let mut definition = McpServerDefinition {
            id: Uuid::new_v4(),
            name: "remote".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                bearer_token_env: Some("MCP_SECRET".into()),
            },
            enabled: true,
        };
        store.save(&definition, true).expect("create");
        assert_eq!(store.enabled().expect("enabled"), [definition.clone()]);

        definition.enabled = false;
        definition.name = "remote-edited".into();
        store.save(&definition, false).expect("update");
        assert!(store.enabled().expect("disabled").is_empty());
        assert_eq!(store.get(definition.id).expect("get"), definition);
        let stored: String = connection
            .query_row("SELECT transport_json FROM mcp_servers", [], |row| {
                row.get(0)
            })
            .expect("transport");
        assert!(stored.contains("MCP_SECRET"));
        assert!(!stored.contains("actual-secret"));

        store.remove(definition.id).expect("delete");
        assert!(matches!(store.get(definition.id), Err(McpError::NotFound)));
    }

    #[test]
    fn catalog_rejects_duplicate_names_and_invalid_http_urls() {
        let connection = store_connection();
        let store = McpStore::new(&connection);
        let first = McpServerDefinition {
            id: Uuid::new_v4(),
            name: "server".into(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1/mcp".into(),
                bearer_token_env: None,
            },
            enabled: true,
        };
        store.save(&first, true).expect("first");
        let duplicate = McpServerDefinition {
            id: Uuid::new_v4(),
            ..first.clone()
        };
        assert!(matches!(
            store.save(&duplicate, true),
            Err(McpError::DuplicateName)
        ));
        let invalid = McpServerDefinition {
            id: Uuid::new_v4(),
            name: "invalid".into(),
            transport: McpTransport::Http {
                url: "ftp://example.com".into(),
                bearer_token_env: None,
            },
            enabled: true,
        };
        assert!(matches!(invalid.validate(), Err(McpError::Validation(_))));
    }

    #[tokio::test]
    async fn runtime_lists_and_calls_mapped_demo_tool() {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            crate::mcp_demo::DemoServer
                .serve(server_transport)
                .await
                .expect("serve")
                .waiting()
                .await
                .expect("wait");
        });
        let service = ().serve(client_transport).await.expect("connect");
        let definition = McpServerDefinition {
            id: Uuid::new_v4(),
            name: "demo".into(),
            transport: McpTransport::Stdio {
                command: "unused".into(),
                args: vec![],
                working_directory: PathBuf::from("."),
            },
            enabled: true,
        };
        let raw_tools = service.peer().list_all_tools().await.expect("tools");
        let mut names = HashSet::new();
        let tools = raw_tools
            .into_iter()
            .enumerate()
            .map(|(index, tool)| {
                let name = unique_function_name(&definition.name, &tool.name, index, &mut names);
                map_tool(&definition, tool, name)
            })
            .collect::<Vec<_>>();
        let function_name = tools[0].function_name.clone();
        let runtime = McpRuntime {
            connections: vec![ConnectedServer {
                definition,
                service,
            }],
            tools,
        };
        assert_eq!(runtime.tools()[0].name, "echo");
        let (_, result) = runtime
            .call(&function_name, json!({"text":"через MCP"}))
            .await
            .expect("call");
        assert_eq!(result["content"][0]["text"], "через MCP");
        drop(runtime);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn streamable_http_connects_and_lists_tools() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        use axum::extract::{Request, State};
        use axum::http::StatusCode;
        use axum::middleware::{self, Next};
        use axum::response::{IntoResponse, Response};
        use rmcp::transport::streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        };

        #[derive(Clone)]
        struct AuthState {
            expected: String,
            seen: Arc<AtomicBool>,
        }

        async fn require_bearer(
            State(state): State<AuthState>,
            request: Request,
            next: Next,
        ) -> Response {
            if request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some(state.expected.as_str())
            {
                state.seen.store(true, Ordering::SeqCst);
                next.run(request).await
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }

        let service: StreamableHttpService<crate::mcp_demo::DemoServer, LocalSessionManager> =
            StreamableHttpService::new(
                || Ok(crate::mcp_demo::DemoServer),
                Default::default(),
                StreamableHttpServerConfig::default(),
            );
        let token = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo test environment");
        let auth_seen = Arc::new(AtomicBool::new(false));
        let router = axum::Router::new().nest_service("/mcp", service).layer(
            middleware::from_fn_with_state(
                AuthState {
                    expected: format!("Bearer {token}"),
                    seen: auth_seen.clone(),
                },
                require_bearer,
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("HTTP server");
        });
        let definition = McpServerDefinition {
            id: Uuid::new_v4(),
            name: "http-demo".into(),
            transport: McpTransport::Http {
                url: format!("http://{address}/mcp"),
                bearer_token_env: Some("CARGO_MANIFEST_DIR".into()),
            },
            enabled: true,
        };
        let tools = inspect_server(&definition)
            .await
            .expect("inspect HTTP server");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert!(auth_seen.load(Ordering::SeqCst));
        server.abort();
    }
}
