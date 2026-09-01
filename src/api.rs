use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::settings::Settings;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const TEMPERATURE: f32 = 0.1;
const BASE_SYSTEM_INSTRUCTION: &str = r#"Ты точный и аккуратный ассистент.
- Отвечай на языке пользователя. Если вопрос задан по-русски, весь содержательный текст должен быть только на русском языке.
- Молча исправляй очевидные опечатки в вопросе.
- Не выдумывай термины, названия и факты. Если не уверен, прямо укажи на неопределённость.
- В справочных ответах избегай ненужных точных чисел, превосходных сравнений и редких названий, если не уверен в них.
- Сначала дай прямой ответ, затем только относящиеся к вопросу детали."#;

fn structured_response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "agi_response",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "short_answer": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Основной ответ в 1–3 предложениях"
                    },
                    "details": {
                        "type": "array",
                        "description": "Только надёжные и относящиеся к вопросу пояснения и факты",
                        "items": { "type": "string", "minLength": 1 },
                        "minItems": 1,
                        "maxItems": 8
                    },
                    "conclusion": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Краткий вывод или следующий шаг"
                    }
                },
                "required": ["short_answer", "details", "conclusion"],
                "additionalProperties": false
            }
        }
    })
}

fn build_system_instruction(settings: &Settings) -> String {
    let mut instruction = format!(
        "{BASE_SYSTEM_INSTRUCTION}\n\nСформируй законченный ответ объёмом не более {} токенов. Если лимит мал, сократи детали, но обязательно заверши мысль.",
        settings.max_tokens()
    );

    if settings.response_format_enabled() {
        instruction.push_str(
            "\n\nStructured Output: заполни каждое поле содержательно. Значения полей должны быть на языке пользователя.",
        );
    }

    if let Some(completion_instruction) = settings.system_instruction() {
        instruction.push_str("\n\n");
        instruction.push_str(&completion_instruction);
    }

    instruction
}

#[derive(Debug)]
pub(crate) struct NeuralDeepClient {
    http: Client,
    api_key: String,
    base_url: String,
    model: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ApiAnswer {
    pub(crate) content: String,
    pub(crate) truncated: bool,
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
    #[error("Structured Output не соответствует ожидаемой JSON Schema: {0}")]
    InvalidStructuredOutput(serde_json::Error),
    #[error(
        "AI-сервис вернул ответ без текста (finish_reason: {finish_reason}); попробуйте увеличить max_tokens в /settings"
    )]
    MissingContent { finish_reason: String },
}

impl NeuralDeepClient {
    pub(crate) fn new(api_key: String, base_url: String, model: String) -> Result<Self, ApiError> {
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(ApiError::BuildClient)?;

        Ok(Self {
            http,
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            model,
        })
    }

    pub(crate) async fn ask(
        &self,
        question: &str,
        settings: &Settings,
    ) -> Result<ApiAnswer, ApiError> {
        let system_instruction = build_system_instruction(settings);
        let messages = vec![
            RequestMessage {
                role: "system",
                content: &system_instruction,
            },
            RequestMessage {
                role: "user",
                content: question,
            },
        ];

        let request = ChatRequest {
            model: &self.model,
            messages,
            max_tokens: settings.max_tokens(),
            response_format: settings
                .response_format_enabled()
                .then(structured_response_format),
            stop: settings.stop_sequence(),
            temperature: TEMPERATURE,
            chat_template_kwargs: ChatTemplateKwargs {
                enable_thinking: false,
            },
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
        let choice =
            response
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| ApiError::MissingContent {
                    finish_reason: "не указан".to_owned(),
                })?;
        let finish_reason = choice.finish_reason;
        let content = choice
            .message
            .content
            .map(|content| content.trim().to_owned())
            .filter(|content| !content.is_empty())
            .ok_or_else(|| ApiError::MissingContent {
                finish_reason: finish_reason
                    .clone()
                    .unwrap_or_else(|| "не указан".to_owned()),
            })?;
        let content = if settings.response_format_enabled() {
            let structured = serde_json::from_str::<StructuredAnswer>(&content)
                .map_err(ApiError::InvalidStructuredOutput)?;
            serde_json::to_string_pretty(&structured)
                .expect("serializing a structured answer should not fail")
        } else {
            content
        };
        let truncated = matches!(finish_reason.as_deref(), Some("length" | "max_tokens"));

        Ok(ApiAnswer { content, truncated })
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<RequestMessage<'a>>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a str>,
    temperature: f32,
    chat_template_kwargs: ChatTemplateKwargs,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
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
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct StructuredAnswer {
    short_answer: String,
    details: Vec<String>,
    conclusion: String,
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
    use std::io::{Cursor, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};

    use super::*;
    use crate::input::BufferedInput;

    #[tokio::test]
    async fn sends_chat_request_and_returns_content() {
        let response_body = r#"{"choices":[{"message":{"content":"{\"conclusion\":\"Лось живёт в северных лесах.\",\"details\":[\"Это крупнейший представитель семейства оленевых.\"],\"short_answer\":\"Лось — крупное млекопитающее.\"}"},"finish_reason":"stop"}]}"#;
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let client = test_client(base_url);
        let mut settings = Settings::default();
        let mut settings_input = BufferedInput::new(Cursor::new("1\nда\n2\n750\n3\n1\n<END>\n0\n"));
        settings
            .configure(&mut settings_input, &mut Vec::new())
            .expect("settings should be configured");

        let answer = client
            .ask("Что такое ownership?", &settings)
            .await
            .expect("request should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");

        assert_eq!(
            answer,
            ApiAnswer {
                content: r#"{
  "short_answer": "Лось — крупное млекопитающее.",
  "details": [
    "Это крупнейший представитель семейства оленевых."
  ],
  "conclusion": "Лось живёт в северных лесах."
}"#
                .to_owned(),
                truncated: false,
            }
        );
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
        assert_eq!(body["model"], "qwen3.8-27b-noreason");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .expect("system instruction should be text")
                .contains("только на русском языке")
        );
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Что такое ownership?");
        assert_eq!(body["max_tokens"], 750);
        assert_eq!(body["temperature"], 0.1);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["required"],
            json!(["short_answer", "details", "conclusion"])
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["additionalProperties"],
            false
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["properties"]["details"]["minItems"],
            1
        );
        assert_eq!(body["stop"], "<END>");
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
        let settings = Settings::default();

        for (status, response_body, expected_message) in cases {
            let (base_url, _request_rx, server) = spawn_server(status, response_body);
            let error = test_client(base_url)
                .ask("test", &settings)
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
        let settings = Settings::default();

        let error = test_client(base_url)
            .ask("test", &settings)
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
            let settings = Settings::default();

            let error = test_client(base_url)
                .ask("test", &settings)
                .await
                .expect_err("missing content should fail");
            server.join().expect("mock server should stop");

            assert!(matches!(error, ApiError::MissingContent { .. }));
        }
    }

    #[tokio::test]
    async fn marks_answer_truncated_by_token_limit() {
        let response_body = r#"{"choices":[{"message":{"content":"Незавершённый ответ"},"finish_reason":"length"}]}"#;
        let (base_url, _request_rx, server) = spawn_server(200, response_body);
        let settings = Settings::default();

        let answer = test_client(base_url)
            .ask("test", &settings)
            .await
            .expect("partial content should be returned");
        server.join().expect("mock server should stop");

        assert_eq!(answer.content, "Незавершённый ответ");
        assert!(answer.truncated);
    }

    fn test_client(base_url: String) -> NeuralDeepClient {
        NeuralDeepClient::new(
            "test-key".to_owned(),
            base_url,
            "qwen3.8-27b-noreason".to_owned(),
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
