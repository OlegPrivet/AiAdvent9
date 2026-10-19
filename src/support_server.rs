//! Read-only CRM for the support demo. All data travels through MCP.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum SupportDataError {
    #[error("Не удалось прочитать {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Некорректный JSON в {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct User {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) environment: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Ticket {
    pub(crate) id: String,
    pub(crate) user_id: String,
    pub(crate) subject: String,
    #[serde(default)]
    pub(crate) environment: serde_json::Value,
    pub(crate) error: String,
    #[serde(default)]
    pub(crate) messages: Vec<String>,
    #[serde(default)]
    pub(crate) attempted_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TicketSummary {
    pub(crate) id: String,
    pub(crate) user_id: String,
    pub(crate) subject: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TicketRequest {
    ticket_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UserRequest {
    user_id: String,
}

#[derive(Clone)]
pub(crate) struct SupportServer {
    directory: PathBuf,
}

pub(crate) fn load(directory: &Path) -> Result<(Vec<User>, Vec<Ticket>), SupportDataError> {
    fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, SupportDataError> {
        let bytes = std::fs::read(path).map_err(|source| SupportDataError::Io {
            path: path.to_owned(),
            source,
        })?;
        serde_json::from_slice(&bytes).map_err(|source| SupportDataError::Json {
            path: path.to_owned(),
            source,
        })
    }
    let users: Vec<User> = read(&directory.join("users.json"))?;
    let tickets: Vec<Ticket> = read(&directory.join("tickets.json"))?;
    let mut user_ids = HashSet::new();
    for user in &users {
        if user.id.trim().is_empty() || !user_ids.insert(user.id.as_str()) {
            return Err(SupportDataError::Invalid(
                "Пустой или повторяющийся ID пользователя".into(),
            ));
        }
    }
    let mut ticket_ids = HashSet::new();
    for ticket in &tickets {
        if ticket.id.trim().is_empty() || !ticket_ids.insert(ticket.id.as_str()) {
            return Err(SupportDataError::Invalid(
                "Пустой или повторяющийся ID тикета".into(),
            ));
        }
        if !user_ids.contains(ticket.user_id.as_str()) {
            return Err(SupportDataError::Invalid(format!(
                "Тикет {} ссылается на неизвестного пользователя",
                ticket.id
            )));
        }
    }
    Ok((users, tickets))
}

fn result<T: Serialize>(value: Result<T, String>) -> CallToolResult {
    match value.and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())) {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
    }
}

impl SupportServer {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
}

#[tool_router]
impl SupportServer {
    #[tool(description = "Получить тикет поддержки agi по ID: окружение, ошибку и переписку")]
    async fn get_ticket(&self, Parameters(request): Parameters<TicketRequest>) -> CallToolResult {
        result(
            load(&self.directory)
                .map_err(|e| e.to_string())
                .and_then(|(_, tickets)| {
                    tickets
                        .into_iter()
                        .find(|t| t.id == request.ticket_id)
                        .ok_or_else(|| format!("Тикет {} не найден", request.ticket_id))
                }),
        )
    }

    #[tool(description = "Получить профиль пользователя поддержки agi по ID")]
    async fn get_user(&self, Parameters(request): Parameters<UserRequest>) -> CallToolResult {
        result(
            load(&self.directory)
                .map_err(|e| e.to_string())
                .and_then(|(users, _)| {
                    users
                        .into_iter()
                        .find(|u| u.id == request.user_id)
                        .ok_or_else(|| format!("Пользователь {} не найден", request.user_id))
                }),
        )
    }

    #[tool(description = "Получить краткий список обращений одного пользователя agi")]
    async fn list_user_tickets(
        &self,
        Parameters(request): Parameters<UserRequest>,
    ) -> CallToolResult {
        result(
            load(&self.directory)
                .map_err(|e| e.to_string())
                .and_then(|(users, tickets)| {
                    if !users.iter().any(|u| u.id == request.user_id) {
                        return Err(format!("Пользователь {} не найден", request.user_id));
                    }
                    Ok(tickets
                        .into_iter()
                        .filter(|t| t.user_id == request.user_id)
                        .map(|t| TicketSummary {
                            id: t.id,
                            user_id: t.user_id,
                            subject: t.subject,
                        })
                        .collect::<Vec<_>>())
                }),
        )
    }
}

#[tool_handler]
impl ServerHandler for SupportServer {}

pub(crate) async fn run(directory: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    SupportServer::new(directory)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mcp::{McpRuntime, McpServerDefinition, McpTransport};
    use serde_json::json;

    pub(crate) struct Fixture {
        pub(crate) directory: PathBuf,
        pub(crate) definition: McpServerDefinition,
        task: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        pub(crate) async fn new() -> Self {
            use rmcp::transport::streamable_http_server::{
                StreamableHttpServerConfig, StreamableHttpService,
                session::local::LocalSessionManager,
            };
            let directory =
                std::env::temp_dir().join(format!("agi-support-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("users.json"),
                include_str!("../projetcDocs/support_demo/users.json"),
            )
            .unwrap();
            std::fs::write(
                directory.join("tickets.json"),
                include_str!("../projetcDocs/support_demo/tickets.json"),
            )
            .unwrap();
            let data = directory.clone();
            let service: StreamableHttpService<SupportServer, LocalSessionManager> =
                StreamableHttpService::new(
                    move || Ok(SupportServer::new(data.clone())),
                    Default::default(),
                    StreamableHttpServerConfig::default(),
                );
            let router = axum::Router::new().nest_service("/mcp", service);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/mcp", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let definition = McpServerDefinition {
                id: uuid::Uuid::new_v4(),
                name: "support-test".into(),
                transport: McpTransport::Http {
                    url,
                    bearer_token_env: None,
                },
                enabled: true,
            };
            Self {
                directory,
                definition,
                task,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn mcp_lists_three_read_only_tools_and_refreshes_json() {
        let fixture = Fixture::new().await;
        let (runtime, errors) =
            McpRuntime::connect(std::slice::from_ref(&fixture.definition)).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(runtime.tools().len(), 3);
        let tool = runtime
            .tools()
            .iter()
            .find(|t| t.name == "get_ticket")
            .unwrap();
        let (_, value) = runtime
            .call(&tool.function_name, json!({"ticket_id":"T-103"}))
            .await
            .unwrap();
        assert_eq!(value["ok"], true);
        assert!(value.to_string().contains("404"));
        let (users, mut tickets) = load(&fixture.directory).unwrap();
        tickets[0].error = "Новая ошибка".into();
        std::fs::write(
            fixture.directory.join("tickets.json"),
            serde_json::to_vec(&tickets).unwrap(),
        )
        .unwrap();
        let (_, value) = runtime
            .call(&tool.function_name, json!({"ticket_id":"T-103"}))
            .await
            .unwrap();
        assert!(value.to_string().contains("Новая ошибка"));
        let (_, value) = runtime
            .call(&tool.function_name, json!({"ticket_id":"missing"}))
            .await
            .unwrap();
        assert_eq!(value["ok"], false);
        assert!(value.to_string().contains("не найден"));
        assert_eq!(users.len(), 3);
    }

    #[tokio::test]
    async fn rejects_malformed_data_duplicate_ids_and_broken_user_references() {
        let fixture = Fixture::new().await;
        let (users, tickets) = load(&fixture.directory).unwrap();
        let mut duplicate = users.clone();
        duplicate.push(users[0].clone());
        std::fs::write(
            fixture.directory.join("users.json"),
            serde_json::to_vec(&duplicate).unwrap(),
        )
        .unwrap();
        assert!(
            load(&fixture.directory)
                .unwrap_err()
                .to_string()
                .contains("ID пользователя")
        );
        std::fs::write(
            fixture.directory.join("users.json"),
            serde_json::to_vec(&users).unwrap(),
        )
        .unwrap();
        let mut duplicate = tickets.clone();
        duplicate.push(tickets[0].clone());
        std::fs::write(
            fixture.directory.join("tickets.json"),
            serde_json::to_vec(&duplicate).unwrap(),
        )
        .unwrap();
        assert!(
            load(&fixture.directory)
                .unwrap_err()
                .to_string()
                .contains("ID тикета")
        );
        let mut broken = tickets;
        broken[0].user_id = "missing".into();
        std::fs::write(
            fixture.directory.join("tickets.json"),
            serde_json::to_vec(&broken).unwrap(),
        )
        .unwrap();
        assert!(
            load(&fixture.directory)
                .unwrap_err()
                .to_string()
                .contains("неизвестного пользователя")
        );
        std::fs::write(fixture.directory.join("tickets.json"), "{").unwrap();
        assert!(
            load(&fixture.directory)
                .unwrap_err()
                .to_string()
                .contains("Некорректный JSON")
        );
    }
}
