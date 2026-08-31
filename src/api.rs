use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(crate) struct NeuralDeepClient {
    http: Client,
    api_key: String,
    base_url: String,
    model: String,
    max_tokens: u32,
}

#[derive(Debug, Error)]
pub(crate) enum ApiError {
    #[error("не удалось настроить HTTP-клиент: {0}")]
    BuildClient(reqwest::Error),
    #[error("не удалось выполнить запрос к AI-сервису: {0}")]
    Request(reqwest::Error),
    #[error("AI-сервис вернул HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("не удалось разобрать ответ AI-сервиса: {0}")]
    InvalidJson(reqwest::Error),
    #[error("AI-сервис вернул ответ без текста")]
    MissingContent,
}

impl NeuralDeepClient {
    pub(crate) fn new(
        api_key: String,
        base_url: String,
        model: String,
        max_tokens: u32,
    ) -> Result<Self, ApiError> {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::BuildClient)?;

        Ok(Self {
            http,
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model,
            max_tokens,
        })
    }

    pub(crate) async fn ask(&self, question: &str) -> Result<String, ApiError> {
        let request = ChatRequest {
            model: &self.model,
            messages: [RequestMessage {
                role: "user",
                content: question,
            }],
            max_tokens: self.max_tokens,
            stream: false,
        };

        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(ApiError::Request)?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.map_err(ApiError::Request)?;
            return Err(ApiError::Http {
                status,
                message: extract_error_message(&body),
            });
        }

        let response = response
            .json::<ChatResponse>()
            .await
            .map_err(ApiError::InvalidJson)?;
        let content = response
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty())
            .ok_or(ApiError::MissingContent)?;

        Ok(content)
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [RequestMessage<'a>; 1],
    max_tokens: u32,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct RequestMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    detail: Option<Value>,
    error: Option<OpenAiError>,
}

#[derive(Debug, Deserialize)]
struct OpenAiError {
    message: Option<String>,
}

fn extract_error_message(body: &str) -> String {
    if let Ok(error) = serde_json::from_str::<ErrorResponse>(body) {
        if let Some(message) = error.error.and_then(|error| error.message) {
            return message;
        }

        if let Some(detail) = error.detail {
            return match detail {
                Value::String(message) => message,
                other => other.to_string(),
            };
        }
    }

    let body = body.trim();
    if body.is_empty() {
        "ответ не содержит описания ошибки".to_owned()
    } else {
        body.chars().take(500).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};

    use super::*;

    #[tokio::test]
    async fn sends_chat_request_and_returns_content() {
        let response_body = r#"{"choices":[{"message":{"content":"Готовый ответ"}}]}"#;
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let client = test_client(base_url);

        let answer = client
            .ask("Что такое ownership?")
            .await
            .expect("request should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");

        assert_eq!(answer, "Готовый ответ");
        assert!(request.starts_with("POST /chat/completions HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-key")
        );

        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");
        assert_eq!(body["model"], "qwen3.8-27b");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Что такое ownership?");
        assert_eq!(body["max_tokens"], 500);
        assert_eq!(body["stream"], false);
    }

    #[tokio::test]
    async fn reports_supported_http_errors() {
        let cases = [
            (401, r#"{"error":{"message":"bad key"}}"#, "bad key"),
            (
                429,
                r#"{"detail":"session limit reached"}"#,
                "session limit reached",
            ),
            (500, "internal error", "internal error"),
        ];

        for (status, response_body, expected_message) in cases {
            let (base_url, _request_rx, server) = spawn_server(status, response_body);
            let error = test_client(base_url)
                .ask("test")
                .await
                .expect_err("request should fail");
            server.join().expect("mock server should stop");

            match error {
                ApiError::Http {
                    status: actual_status,
                    message,
                } => {
                    assert_eq!(actual_status.as_u16(), status);
                    assert_eq!(message, expected_message);
                }
                other => panic!("unexpected error: {other}"),
            }
        }
    }

    #[tokio::test]
    async fn rejects_invalid_json() {
        let (base_url, _request_rx, server) = spawn_server(200, "not-json");

        let error = test_client(base_url)
            .ask("test")
            .await
            .expect_err("invalid JSON should fail");
        server.join().expect("mock server should stop");

        assert!(matches!(error, ApiError::InvalidJson(_)));
    }

    #[tokio::test]
    async fn rejects_response_without_content() {
        for response_body in [
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{"content":null}}]}"#,
            r#"{"choices":[{"message":{"content":"   "}}]}"#,
        ] {
            let (base_url, _request_rx, server) = spawn_server(200, response_body);

            let error = test_client(base_url)
                .ask("test")
                .await
                .expect_err("missing content should fail");
            server.join().expect("mock server should stop");

            assert!(matches!(error, ApiError::MissingContent));
        }
    }

    fn test_client(base_url: String) -> NeuralDeepClient {
        NeuralDeepClient::new(
            "test-key".to_owned(),
            base_url,
            "qwen3.8-27b".to_owned(),
            500,
        )
        .expect("client should be built")
    }

    fn spawn_server(
        status: u16,
        response_body: &str,
    ) -> (String, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let response_body = response_body.to_owned();
        let (request_tx, request_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server should accept");
            let request = read_request(&mut stream);
            request_tx.send(request).expect("request should be sent");

            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Test Response",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock response should be written");
        });

        (format!("http://{address}"), request_rx, server)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];

        loop {
            let read = stream.read(&mut buffer).expect("request should be read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);

            if request_is_complete(&request) {
                break;
            }
        }

        String::from_utf8(request).expect("request should be UTF-8")
    }

    fn request_is_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);

        request.len() >= header_end + 4 + content_length
    }
}
