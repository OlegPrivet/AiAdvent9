use std::io;
use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::chat::ChatMessage;
use crate::settings::Settings;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const TEMPERATURE: f32 = 0.1;

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
    #[cfg(test)]
    #[error("не удалось разобрать ответ AI-сервиса: {0}")]
    InvalidJson(reqwest::Error),
    #[error("не удалось разобрать фрагмент потокового ответа: {0}")]
    InvalidStream(serde_json::Error),
    #[error("потоковый ответ завершился без маркера [DONE]")]
    IncompleteStream,
    #[error("не удалось вывести потоковый ответ: {0}")]
    Output(io::Error),
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

    #[cfg(test)]
    pub(crate) async fn ask(
        &self,
        history: &[ChatMessage],
        chat_id: Uuid,
        question: &str,
        settings: &Settings,
    ) -> Result<ApiAnswer, ApiError> {
        let response = self
            .send_request(history, chat_id, question, settings, false)
            .await?;
        let response = response
            .json::<ChatResponse>()
            .await
            .map_err(ApiError::InvalidJson)?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| missing_content(None))?;

        finish_answer(
            choice.message.content.unwrap_or_default(),
            choice.finish_reason,
            settings,
        )
    }

    pub(crate) async fn ask_streaming<F>(
        &self,
        history: &[ChatMessage],
        chat_id: Uuid,
        question: &str,
        settings: &Settings,
        mut on_delta: F,
    ) -> Result<ApiAnswer, ApiError>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        let mut response = self
            .send_request(history, chat_id, question, settings, true)
            .await?;
        let mut decoder = SseDecoder::default();
        let mut content = String::new();
        let mut finish_reason = None;
        let mut done = false;

        'response: while let Some(chunk) = response.chunk().await.map_err(ApiError::Request)? {
            for data in decoder.push(&chunk) {
                if data == b"[DONE]" {
                    done = true;
                    break 'response;
                }
                consume_stream_chunk(&data, &mut content, &mut finish_reason, &mut on_delta)?;
            }
        }

        if !done {
            for data in decoder.finish() {
                if data == b"[DONE]" {
                    done = true;
                    break;
                }
                consume_stream_chunk(&data, &mut content, &mut finish_reason, &mut on_delta)?;
            }
        }

        if !done {
            return Err(ApiError::IncompleteStream);
        }

        finish_answer(content, finish_reason, settings)
    }

    async fn send_request(
        &self,
        history: &[ChatMessage],
        chat_id: Uuid,
        question: &str,
        settings: &Settings,
        stream: bool,
    ) -> Result<Response, ApiError> {
        let system_instruction = settings.effective_system_prompt();
        let mut messages = Vec::with_capacity(history.len() + 2);
        if let Some(system_instruction) = system_instruction.as_deref() {
            messages.push(RequestMessage {
                role: "system",
                content: system_instruction,
            });
        }
        messages.extend(history.iter().map(|message| RequestMessage {
            role: message.role.as_api_str(),
            content: &message.content,
        }));
        messages.push(RequestMessage {
            role: "user",
            content: question,
        });
        let user = chat_id.to_string();

        let request = ChatRequest {
            model: &self.model,
            messages,
            max_tokens: settings.max_tokens(),
            response_format: settings
                .response_format_enabled()
                .then(structured_response_format),
            stop: settings.stop_sequence(),
            temperature: TEMPERATURE,
            user: &user,
            chat_template_kwargs: ChatTemplateKwargs {
                enable_thinking: false,
            },
            stream,
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

        Ok(response)
    }
}

fn consume_stream_chunk<F>(
    data: &[u8],
    content: &mut String,
    finish_reason: &mut Option<String>,
    on_delta: &mut F,
) -> Result<(), ApiError>
where
    F: FnMut(&str) -> io::Result<()>,
{
    let chunk = serde_json::from_slice::<StreamChunk>(data).map_err(ApiError::InvalidStream)?;

    for choice in chunk.choices {
        if let Some(reason) = choice.finish_reason {
            *finish_reason = Some(reason);
        }
        if let Some(delta) = choice.delta.content.filter(|value| !value.is_empty()) {
            on_delta(&delta).map_err(ApiError::Output)?;
            content.push_str(&delta);
        }
    }

    Ok(())
}

fn finish_answer(
    content: String,
    finish_reason: Option<String>,
    settings: &Settings,
) -> Result<ApiAnswer, ApiError> {
    let content = content.trim().to_owned();
    if content.is_empty() {
        return Err(missing_content(finish_reason));
    }

    let content = if settings.response_format_enabled() {
        let structured = serde_json::from_str::<StructuredAnswer>(&content)
            .map_err(ApiError::InvalidStructuredOutput)?;
        serde_json::to_string_pretty(&structured).map_err(ApiError::InvalidStructuredOutput)?
    } else {
        content
    };
    let truncated = matches!(finish_reason.as_deref(), Some("length" | "max_tokens"));

    Ok(ApiAnswer { content, truncated })
}

fn missing_content(finish_reason: Option<String>) -> ApiError {
    ApiError::MissingContent {
        finish_reason: finish_reason.unwrap_or_else(|| "не указан".to_owned()),
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
    user: &'a str,
    chat_template_kwargs: ChatTemplateKwargs,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

#[derive(Debug, Serialize)]
struct RequestMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
}

#[derive(Debug, Default)]
struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        self.take_complete_lines(false)
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        self.take_complete_lines(true)
    }

    fn take_complete_lines(&mut self, include_remainder: bool) -> Vec<Vec<u8>> {
        let mut data_lines = Vec::new();

        while let Some(line_end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=line_end).collect::<Vec<_>>();
            if let Some(data) = parse_sse_data_line(&line) {
                data_lines.push(data);
            }
        }

        if include_remainder && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            if let Some(data) = parse_sse_data_line(&line) {
                data_lines.push(data);
            }
        }

        data_lines
    }
}

fn parse_sse_data_line(line: &[u8]) -> Option<Vec<u8>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let data = line.strip_prefix(b"data:")?;
    Some(data.strip_prefix(b" ").unwrap_or(data).to_vec())
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
        let mut settings_input =
            BufferedInput::new(Cursor::new("1\n1\n2\n750\n3\n2\n<END>\nesc\n"));
        settings
            .configure(&mut settings_input, &mut Vec::new())
            .expect("settings should be configured");

        let answer = client
            .ask(&[], Uuid::nil(), "Что такое ownership?", &settings)
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
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Что такое ownership?");
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
        assert_eq!(body["user"], Uuid::nil().to_string());
        assert_eq!(body["stream"], false);
    }

    #[tokio::test]
    async fn streams_sse_deltas_and_returns_complete_answer() {
        let response_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Привет, \"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"мир!\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":null},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let settings = Settings::default();
        let mut streamed = String::new();

        let answer = test_client(base_url)
            .ask_streaming(&[], Uuid::nil(), "test", &settings, |delta| {
                streamed.push_str(delta);
                Ok(())
            })
            .await
            .expect("stream should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");

        assert_eq!(streamed, "Привет, мир!");
        assert_eq!(answer.content, "Привет, мир!");
        assert!(!answer.truncated);
        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn sends_full_chat_history_custom_prompt_and_session_id() {
        let response_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Продолжение\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let client = test_client(base_url);
        let chat_id = Uuid::new_v4();
        let history = vec![
            ChatMessage {
                role: crate::chat::MessageRole::User,
                content: "Первый вопрос".to_owned(),
            },
            ChatMessage {
                role: crate::chat::MessageRole::Assistant,
                content: "Первый ответ".to_owned(),
            },
        ];
        let mut settings = Settings::default();
        let mut settings_input =
            BufferedInput::new(Cursor::new("4\n1\nТолько мой системный prompt\nesc\n"));
        settings
            .configure(&mut settings_input, &mut Vec::new())
            .expect("custom prompt should be configured");

        client
            .ask_streaming(&history, chat_id, "Следующий вопрос", &settings, |_| Ok(()))
            .await
            .expect("stream should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");
        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");

        assert_eq!(body["user"], chat_id.to_string());
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(4));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"],
            "Только мой системный prompt"
        );
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Первый вопрос");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "Первый ответ");
        assert_eq!(body["messages"][3]["role"], "user");
        assert_eq!(body["messages"][3]["content"], "Следующий вопрос");
    }

    #[tokio::test]
    async fn omits_system_prompt_from_request_by_default() {
        let response_body =
            r#"{"choices":[{"message":{"content":"Ответ"},"finish_reason":"stop"}]}"#;
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let settings = Settings::default();

        test_client(base_url)
            .ask(&[], Uuid::nil(), "Вопрос без системного prompt", &settings)
            .await
            .expect("request should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");
        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");

        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            body["messages"][0]["content"],
            "Вопрос без системного prompt"
        );
    }

    #[tokio::test]
    async fn rejects_malformed_or_incomplete_sse() {
        let cases = [
            ("data: not-json\n\ndata: [DONE]\n\n", "malformed"),
            (
                "data: {\"choices\":[{\"delta\":{\"content\":\"часть\"},\"finish_reason\":null}]}\n\n",
                "incomplete",
            ),
        ];

        for (response_body, expected) in cases {
            let (base_url, _request_rx, server) = spawn_server(200, response_body);
            let settings = Settings::default();
            let error = test_client(base_url)
                .ask_streaming(&[], Uuid::nil(), "test", &settings, |_| Ok(()))
                .await
                .expect_err("invalid SSE should fail");
            server.join().expect("mock server should stop");

            match expected {
                "malformed" => assert!(matches!(error, ApiError::InvalidStream(_))),
                "incomplete" => assert!(matches!(error, ApiError::IncompleteStream)),
                _ => panic!("unknown test case"),
            }
        }
    }

    #[test]
    fn decodes_sse_lines_split_across_network_chunks() {
        let mut decoder = SseDecoder::default();

        assert!(decoder.push(b"data: {\"choices\":").is_empty());
        let lines = decoder.push(b"[]}\r\ndata: [DO");
        assert_eq!(lines, [br#"{"choices":[]}"#.to_vec()]);
        assert_eq!(decoder.push(b"NE]\r\n"), [b"[DONE]".to_vec()]);
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
                .ask(&[], Uuid::nil(), "test", &settings)
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
            .ask(&[], Uuid::nil(), "test", &settings)
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
                .ask(&[], Uuid::nil(), "test", &settings)
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
            .ask(&[], Uuid::nil(), "test", &settings)
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
