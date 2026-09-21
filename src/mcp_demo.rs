use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct EchoRequest {
    /// Текст, который нужно вернуть без изменений.
    text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DemoServer;

#[tool_router]
impl DemoServer {
    #[tool(description = "Возвращает переданный текст без изменений")]
    async fn echo(&self, Parameters(request): Parameters<EchoRequest>) -> String {
        request.text
    }
}

#[tool_handler]
impl ServerHandler for DemoServer {}

pub(crate) async fn run() -> Result<(), Box<dyn std::error::Error>> {
    DemoServer
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolRequestParams;

    #[tokio::test]
    async fn demo_server_lists_and_calls_echo_tool() {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            DemoServer
                .serve(server_transport)
                .await
                .expect("serve")
                .waiting()
                .await
                .expect("wait");
        });
        let mut client = ().serve(client_transport).await.expect("connect");
        let tools = client.peer().list_all_tools().await.expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = client
            .call_tool(CallToolRequestParams::new("echo").with_arguments(
                serde_json::from_value(serde_json::json!({"text":"привет"})).expect("arguments"),
            ))
            .await
            .expect("call echo");
        assert_eq!(
            result.content[0].as_text().map(|text| text.text.as_str()),
            Some("привет")
        );
        client.close().await.expect("close");
        server.await.expect("server task");
    }
}
